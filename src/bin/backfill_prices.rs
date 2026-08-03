//! One-off backfill: recompute `cost_usd`, `cache_price_impact_usd`, and
//! `cost_confidence` for historical flows that terminalized before a `price_table`
//! was configured.
//!
//! Cost is sealed at flow terminal and persisted; adding prices later only prices
//! *future* flows. This binary walks the durable SQLite history, finds
//! `terminal_facts` rows with `cost_usd IS NULL` whose served model has a
//! configured price, recomputes the figures with the SAME `cost_for_usage` the
//! engine uses at terminal, and rewrites both the `terminal_facts` columns and the
//! CBOR `summary` blobs in `terminal_facts` + `flows_latest` (the dashboard decodes
//! the displayed cost from the blob, not the column — see `dashboard_history.rs`).
//!
//! Reuses `llmconduit::dashboard_api::cost_for_usage` (zero formula drift) and the
//! `SnapshotFlowSummary` / `FlowUsage` / `TerminalCostConfidence` types so the
//! rewritten blobs are byte-compatible with what the live service writes.
//!
//! USAGE:
//!   backfill_prices --config <config.yaml> --db <history.sqlite3>
//!                    [--model <served-model-id>] [--apply]
//!                    [--limit <n>] [--sample <n>]
//!
//! `--limit` caps how many rows are BACKFILLED (default: all of them) — use it
//! for a cautious first `--apply`. `--sample` only caps how many rows are
//! PRINTED (default 8) and never affects what is written. A `--limit` that
//! actually truncates the plan is reported on stdout; the cap is never silent.
//! Row order is `ORDER BY api_call_id`, so a truncated run is reproducible and
//! re-running picks up where it left off (backfilled rows no longer match
//! `cost_usd IS NULL`).
//!
//! Default = dry run (prints plan + sample, writes nothing). Pass `--apply` to
//! write; the binary refuses `--apply` while the gateway port is accepting
//! connections (stop the service first). Does NOT touch `flow_events` /
//! `flow_versions` replay snapshots by default — as-of cursors keep the original
//! `—` (documented in stdout).

use llmconduit::config::{Config, load_persisted_config};
use llmconduit::dashboard_api::cost_for_usage;
use llmconduit::dashboard_flow::{
    FlowUsage, SnapshotFlowSummary, TerminalCostConfidence, normalize_usage,
};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

struct Args {
    config: PathBuf,
    db: PathBuf,
    model: Option<String>,
    apply: bool,
    /// Max rows to actually backfill. `None` = every matching row.
    limit: Option<usize>,
    /// Max rows to print in the plan preview. Never affects what is written.
    sample: usize,
}

/// One `terminal_facts` row in need of pricing, as read from the database.
/// Factored out of the `query_map` closure so the row type stays legible (and
/// clippy's `type_complexity` stays quiet about a seven-field tuple).
struct TerminalRow {
    api_call_id: String,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
    cached_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    summary: Vec<u8>,
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => std::process::exit(code),
    };
    if let Err(err) = run(&args) {
        eprintln!("backfill failed: {err}");
        std::process::exit(1);
    }
}

fn parse_args() -> Result<Args, i32> {
    let mut config: Option<PathBuf> = None;
    let mut db: Option<PathBuf> = None;
    let mut model: Option<String> = None;
    let mut apply = false;
    let mut limit: Option<usize> = None;
    let mut sample: usize = 8;

    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        let s = arg.to_string_lossy().into_owned();
        match s.as_str() {
            "--config" => config = it.next().map(PathBuf::from),
            "--db" => db = it.next().map(PathBuf::from),
            "--model" => model = it.next().map(|s| s.to_string_lossy().into_owned()),
            "--apply" => apply = true,
            // A malformed count is an error, not a silent fallback to some
            // default: `--limit banana` must never be read as "write all rows".
            "--limit" => limit = Some(parse_count(it.next(), "--limit")?),
            "--sample" => sample = parse_count(it.next(), "--sample")?,
            "-h" | "--help" => {
                eprintln!(
                    "usage: backfill_prices --config <yaml> --db <sqlite> \
                     [--model <id>] [--apply] [--limit <n>] [--sample <n>]\n\
                     \n  --limit <n>   backfill at most n rows (default: all)\
                     \n  --sample <n>  print at most n rows (default: 8; never affects writes)"
                );
                return Err(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                return Err(2);
            }
        }
    }

    let config = config.ok_or_else(|| {
        eprintln!("--config is required");
        2_i32
    })?;
    let db = db.ok_or_else(|| {
        eprintln!("--db is required");
        2_i32
    })?;
    Ok(Args {
        config,
        db,
        model,
        apply,
        limit,
        sample,
    })
}

fn parse_count(value: Option<std::ffi::OsString>, flag: &str) -> Result<usize, i32> {
    let value = value.ok_or_else(|| {
        eprintln!("{flag} requires a count");
        2_i32
    })?;
    value.to_string_lossy().parse().map_err(|_| {
        eprintln!("{flag} expects a non-negative integer, got {value:?}");
        2_i32
    })
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    // Safety: refuse --apply while the gateway is up (concurrent writers on the same
    // blobs/rows). Dry-run is always allowed.
    if args.apply && service_is_up() {
        return Err(
            "gateway port 127.0.0.1:5022 is accepting connections — stop the service \
             (`sudo systemctl stop llmconduit`) before --apply"
                .into(),
        );
    }

    let persisted = load_persisted_config(&args.config)?;
    let config = Config::from_persisted(&persisted)?;
    if config.price_table.is_empty() {
        println!("price_table is empty — nothing to backfill.");
        return Ok(());
    }

    // Scope: either the one requested model, or every configured price key.
    let scoped: Vec<&String> = if let Some(m) = &args.model {
        config
            .price_table
            .keys()
            .filter(|k| k.eq_ignore_ascii_case(m))
            .collect()
    } else {
        config.price_table.keys().collect()
    };
    if scoped.is_empty() {
        println!(
            "no price_table entry matches model {:?}",
            args.model.as_deref().unwrap_or("")
        );
        return Ok(());
    }

    println!("config:    {}", args.config.display());
    println!("db:        {}", args.db.display());
    println!(
        "models:    {}",
        scoped
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "mode:      {}",
        if args.apply {
            "APPLY (write)"
        } else {
            "DRY RUN (no writes)"
        }
    );
    println!();

    let mut conn = Connection::open(&args.db)?;
    // Terminal_facts.summary + flows_latest.summary are both rewritten.
    // flow_events / flow_versions (replay snapshots) are intentionally left as-is.

    let mut plan: Vec<RowUpdate> = Vec::new();
    for model_key in &scoped {
        // COLLATE NOCASE matches the case-insensitive lookup `price_for` already
        // performs. Without it a `price_table` key whose casing differs from the
        // served id selects nothing and the backfill silently reports "no rows"
        // — the same class of mismatch that once made a `DeepSeek-V4-Flash`
        // profile key match nothing against the served `DeepSeek-V4-Flash-0731`.
        // ORDER BY makes a `--limit`-truncated run reproducible.
        let mut stmt = conn.prepare(
            "SELECT api_call_id, prompt_tokens, completion_tokens, total_tokens,
                    cached_tokens, reasoning_tokens, summary
             FROM terminal_facts
             WHERE model_served = ?1 COLLATE NOCASE
               AND cost_usd IS NULL AND prompt_tokens IS NOT NULL
             ORDER BY api_call_id",
        )?;
        let rows: Vec<TerminalRow> = stmt
            .query_map(params![model_key.as_str()], |r| {
                Ok(TerminalRow {
                    api_call_id: r.get(0)?,
                    prompt_tokens: r.get(1)?,
                    completion_tokens: r.get(2)?,
                    total_tokens: r.get(3)?,
                    cached_tokens: r.get(4)?,
                    reasoning_tokens: r.get(5)?,
                    summary: r.get(6)?,
                })
            })?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        for row in rows {
            let TerminalRow {
                api_call_id,
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
                cached_tokens: cached,
                reasoning_tokens: reasoning,
                summary: summary_blob,
            } = row;
            let Some(prompt) = prompt else { continue };
            let Some(completion) = completion else {
                continue;
            };
            let usage = FlowUsage {
                prompt,
                completion,
                total: total.unwrap_or(prompt.saturating_add(completion)),
                cached,
                reasoning,
            };
            // Case-insensitive lookup covers a key/serve casing mismatch.
            let Some(price) = config.price_for(model_key) else {
                continue;
            };

            let cost = cost_for_usage(usage, price);

            // Confidence mirror of engine.rs prepare_terminal_pricing:
            //   cached == Some(0)               -> Confident
            //   cached_price_configured         -> Confident
            //   else                            -> Estimated
            let confidence = match usage.cached {
                Some(0) => TerminalCostConfidence::Confident,
                Some(_) | None if price.cached_price_configured => {
                    TerminalCostConfidence::Confident
                }
                Some(_) | None => TerminalCostConfidence::Estimated,
            };

            // cache_price_impact_usd mirror of engine.rs:882 (only when configured
            // AND a cached count was reported).
            let cache_impact = if price.cached_price_configured {
                normalize_usage(usage).usage.cached.map(|cached| {
                    cached as f64 / 1000.0 * (price.cached_per_1k - price.input_per_1k)
                })
            } else {
                None
            };

            let mut summary: SnapshotFlowSummary = serde_cbor::from_slice(&summary_blob)
                .map_err(|e| format!("decode summary for {api_call_id}: {e}"))?;
            summary.terminal_cost_usd = Some(cost);
            summary.terminal_cost_confidence = confidence;
            summary.cache_price_impact_usd = cache_impact;
            let new_blob = serde_cbor::to_vec(&summary)
                .map_err(|e| format!("encode summary for {api_call_id}: {e}"))?;

            plan.push(RowUpdate {
                api_call_id,
                model: model_key.to_string(),
                usage,
                cost,
                confidence_key: confidence_key(confidence),
                cache_impact,
                new_blob,
            });
        }
    }

    if plan.is_empty() {
        println!("no backfillable rows found (all priced or no token counts).");
        return Ok(());
    }

    // A `--limit` that truncates is announced, never silent: a run that says
    // nothing about the rows it dropped reads as "everything is priced now".
    let matched = plan.len();
    if let Some(limit) = args.limit
        && limit < matched
    {
        plan.truncate(limit);
        println!(
            "LIMIT: {matched} row(s) match; --limit {limit} keeps the first {limit} \
             by api_call_id and leaves {} unpriced. Re-run to continue.",
            matched - limit
        );
        println!();
    }

    // Per-model counts + sample.
    let mut by_model: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &plan {
        *by_model.entry(r.model.as_str()).or_insert(0) += 1;
    }
    for (m, n) in &by_model {
        println!("{m}: {n} rows");
    }
    println!();
    println!("sample (up to {}):", args.sample.min(plan.len()));
    for r in plan.iter().take(args.sample) {
        println!(
            "  {} prompt={} completion={} cached={:?} -> cost=${:.6} {} (cache_impact={})",
            short(&r.api_call_id, 20),
            r.usage.prompt,
            r.usage.completion,
            r.usage.cached,
            r.cost,
            r.confidence_key,
            opt_f(r.cache_impact),
        );
    }
    println!();
    println!(
        "note: flow_events / flow_versions replay snapshots are NOT rewritten; \
         as-of cursors keep the original null cost."
    );

    if !args.apply {
        println!();
        println!(
            "dry run — re-run with --apply to write {} row(s).",
            plan.len()
        );
        return Ok(());
    }

    let tx = conn.transaction()?;
    let mut updated_terminal = 0usize;
    let mut updated_latest = 0usize;
    {
        let mut t_stmt = tx.prepare(
            "UPDATE terminal_facts
             SET cost_usd = ?1, cost_confidence = ?2, cache_price_impact_usd = ?3, summary = ?4
             WHERE api_call_id = ?5",
        )?;
        let mut l_stmt =
            tx.prepare("UPDATE flows_latest SET summary = ?1 WHERE api_call_id = ?2")?;
        for r in &plan {
            t_stmt.execute(params![
                r.cost,
                r.confidence_key,
                r.cache_impact,
                r.new_blob,
                r.api_call_id,
            ])?;
            updated_terminal += 1;
            // flows_latest may be absent for purged/very-old flows; count only actual hits.
            let n = l_stmt.execute(params![r.new_blob, r.api_call_id])?;
            updated_latest += n;
        }
    }
    tx.commit()?;

    println!();
    println!(
        "applied: {} terminal_facts row(s) updated, {} flows_latest row(s) updated.",
        updated_terminal, updated_latest
    );
    Ok(())
}

struct RowUpdate {
    api_call_id: String,
    model: String,
    usage: FlowUsage,
    cost: f64,
    confidence_key: &'static str,
    cache_impact: Option<f64>,
    new_blob: Vec<u8>,
}

fn confidence_key(c: TerminalCostConfidence) -> &'static str {
    match c {
        TerminalCostConfidence::Confident => "confident",
        TerminalCostConfidence::Estimated => "estimated",
        TerminalCostConfidence::Unavailable => "unavailable",
    }
}

fn service_is_up() -> bool {
    TcpStream::connect_timeout(
        &"127.0.0.1:5022".parse().unwrap(),
        Duration::from_millis(300),
    )
    .is_ok()
}

fn short(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn opt_f(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("${:.6}", x),
        None => "none".to_string(),
    }
}
