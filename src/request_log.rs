use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

pub fn analyze_request_log(path: &Path, max_pairs: usize) -> Result<String, String> {
    let contents = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let mut entries = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(line).map_err(|err| {
            format!(
                "failed to parse JSON on line {} of {}: {err}",
                index + 1,
                path.display()
            )
        })?;
        entries.push(RequestLogEntry {
            line_number: index + 1,
            raw: line.to_string(),
            value,
        });
    }

    if entries.is_empty() {
        return Ok(format!("{} is empty", path.display()));
    }
    if entries.len() == 1 {
        return Ok(format!(
            "{} contains only one request; collect at least two requests to diff prefixes",
            path.display()
        ));
    }

    let pair_limit = max_pairs.max(1);
    let mut reports = Vec::new();
    let mut path_counts = BTreeMap::<String, usize>::new();
    for window in entries.windows(2).take(pair_limit) {
        let previous = &window[0];
        let current = &window[1];
        let common_prefix_bytes =
            common_prefix_len(previous.raw.as_bytes(), current.raw.as_bytes());
        let mut differing_paths = Vec::new();
        diff_values("$", &previous.value, &current.value, &mut differing_paths);
        for path in &differing_paths {
            *path_counts.entry(path.clone()).or_default() += 1;
        }
        reports.push(PairReport {
            previous_line: previous.line_number,
            current_line: current.line_number,
            previous_len: previous.raw.len(),
            current_len: current.raw.len(),
            common_prefix_bytes,
            differing_paths,
            previous_boundary_preview: boundary_preview(&previous.raw, common_prefix_bytes),
            current_boundary_preview: boundary_preview(&current.raw, common_prefix_bytes),
        });
    }

    let mut output = String::new();
    writeln!(&mut output, "request log: {}", path.display()).expect("write string");
    writeln!(
        &mut output,
        "parsed {} request(s); reporting {} consecutive pair(s)",
        entries.len(),
        reports.len()
    )
    .expect("write string");

    if let Some(worst_pair) = reports
        .iter()
        .min_by_key(|report| report.common_prefix_bytes)
    {
        writeln!(
            &mut output,
            "worst prefix stability: lines {} -> {} share {} byte(s)",
            worst_pair.previous_line, worst_pair.current_line, worst_pair.common_prefix_bytes
        )
        .expect("write string");
    }

    for report in reports {
        writeln!(&mut output).expect("write string");
        writeln!(
            &mut output,
            "lines {} -> {}: common prefix {} byte(s) ({:.1}% of previous, {:.1}% of current)",
            report.previous_line,
            report.current_line,
            report.common_prefix_bytes,
            percent(report.common_prefix_bytes, report.previous_len),
            percent(report.common_prefix_bytes, report.current_len),
        )
        .expect("write string");
        if report.differing_paths.is_empty() {
            writeln!(
                &mut output,
                "  changed paths: none (byte diff likely due to key ordering/formatting)"
            )
            .expect("write string");
        } else {
            writeln!(
                &mut output,
                "  changed paths ({}): {}",
                report.differing_paths.len(),
                report.differing_paths.join(", ")
            )
            .expect("write string");
        }
        writeln!(
            &mut output,
            "  previous boundary: {}",
            report.previous_boundary_preview
        )
        .expect("write string");
        writeln!(
            &mut output,
            "  current boundary:  {}",
            report.current_boundary_preview
        )
        .expect("write string");
    }

    if !path_counts.is_empty() {
        writeln!(&mut output).expect("write string");
        writeln!(&mut output, "most frequent unstable paths:").expect("write string");
        let mut path_counts: Vec<_> = path_counts.into_iter().collect();
        path_counts.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        for (path, count) in path_counts.into_iter().take(10) {
            writeln!(&mut output, "  {path}: {count} pair(s)").expect("write string");
        }
    }

    Ok(output)
}

struct RequestLogEntry {
    line_number: usize,
    raw: String,
    value: Value,
}

struct PairReport {
    previous_line: usize,
    current_line: usize,
    previous_len: usize,
    current_len: usize,
    common_prefix_bytes: usize,
    differing_paths: Vec<String>,
    previous_boundary_preview: String,
    current_boundary_preview: String,
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

fn percent(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 / denominator as f64) * 100.0
    }
}

fn boundary_preview(text: &str, boundary: usize) -> String {
    let left_context_chars = 32usize;
    let right_context_chars = 48usize;
    let boundary_char = text
        .char_indices()
        .take_while(|(index, _)| *index < boundary)
        .count();
    let chars: Vec<char> = text.chars().collect();
    let start = boundary_char.saturating_sub(left_context_chars);
    let end = (boundary_char + right_context_chars).min(chars.len());
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < chars.len() { "…" } else { "" };
    let before: String = chars[start..boundary_char.min(chars.len())]
        .iter()
        .collect();
    let after: String = chars[boundary_char.min(chars.len())..end].iter().collect();
    format!(
        "{prefix}{}|{}{suffix}",
        sanitize_preview(&before),
        sanitize_preview(&after)
    )
}

fn sanitize_preview(value: &str) -> String {
    value
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn diff_values(path: &str, left: &Value, right: &Value, differences: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(left_map), Value::Object(right_map)) => {
            for (key, left_value) in left_map {
                let child_path = format!("{path}.{key}");
                match right_map.get(key) {
                    Some(right_value) => {
                        diff_values(&child_path, left_value, right_value, differences)
                    }
                    None => differences.push(child_path),
                }
            }
            for key in right_map.keys() {
                if !left_map.contains_key(key) {
                    differences.push(format!("{path}.{key}"));
                }
            }
        }
        (Value::Array(left_items), Value::Array(right_items)) => {
            let common_len = left_items.len().min(right_items.len());
            for index in 0..common_len {
                diff_values(
                    &format!("{path}[{index}]"),
                    &left_items[index],
                    &right_items[index],
                    differences,
                );
            }
            if left_items.len() != right_items.len() {
                differences.push(format!("{path}[len]"));
            }
        }
        _ if left != right => differences.push(path.to_string()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::analyze_request_log;
    use super::common_prefix_len;
    use super::diff_values;
    use serde_json::json;

    #[test]
    fn common_prefix_len_counts_matching_bytes() {
        assert_eq!(common_prefix_len(b"abcdef", b"abcxyz"), 3);
        assert_eq!(common_prefix_len(b"", b"abc"), 0);
    }

    #[test]
    fn diff_values_reports_nested_paths() {
        let left = json!({
            "messages": [
                {"role": "user", "content": "hello"}
            ],
            "tools": []
        });
        let right = json!({
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "world"}
            ],
            "tools": [{"type": "function"}]
        });
        let mut diffs = Vec::new();

        diff_values("$", &left, &right, &mut diffs);

        assert_eq!(diffs, vec!["$.messages[len]", "$.tools[len]"]);
    }

    use super::boundary_preview;

    #[test]
    fn request_log_single_entry() {
        let path = std::env::temp_dir().join(format!(
            "resp2chat-single-entry-{}.jsonl",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&path, "{\"model\":\"test\",\"messages\":[]}\n").expect("write");
        let result = analyze_request_log(&path, 10).expect("analyze");
        assert!(result.contains("contains only one request"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn boundary_preview_at_start() {
        let result = boundary_preview("abcdef", 0);
        assert!(result.starts_with('|'));
    }

    #[test]
    fn analyze_request_log_reports_pair_details() {
        let path = std::env::temp_dir().join(format!(
            "resp2chat-request-log-{}.jsonl",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(
            &path,
            concat!(
                "{\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}],\"model\":\"Qwen3.5\"}\n",
                "{\"messages\":[{\"role\":\"user\",\"content\":\"hello\"},{\"role\":\"assistant\",\"content\":\"world\"}],\"model\":\"Qwen3.5\"}\n"
            ),
        )
        .expect("write request log");

        let report = analyze_request_log(&path, 10).expect("analyze request log");

        assert!(report.contains("lines 1 -> 2"));
        assert!(report.contains("$.messages[len]"));
        assert!(report.contains("worst prefix stability"));

        let _ = std::fs::remove_file(path);
    }
}
