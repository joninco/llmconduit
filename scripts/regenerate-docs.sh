#!/usr/bin/env bash
set -euo pipefail
# Regenerate all docs from source code.
# Usage: ./scripts/regenerate-docs.sh [section]
#   section: routes|adapters|engine|config|config.example|upstream|tools|observability|cli|dashboard|all (default)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
DOCS_DIR="$REPO_DIR/docs"
cd "$REPO_DIR"

regenerate_routes() {
  echo "=== regenerating routes.md ==="
  cat <<'EOF' > /dev/null
  # Agent should read src/http.rs and produce routes.md
EOF
  # In practice, run an agent to do this
  echo "To regenerate, run: claude -p 'Regenerate docs/routes.md from src/http.rs per docs/regenerate.md'"
}

regenerate_adapters() {
  echo "=== regenerating adapters.md ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/adapters.md from adapter sources per docs/regenerate.md'"
}

regenerate_engine() {
  echo "=== regenerating engine.md ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/engine.md from src/engine.rs per docs/regenerate.md'"
}

regenerate_config() {
  echo "=== regenerating config.md ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/config.md from src/config.rs per docs/regenerate.md'"
}

regenerate_config_example() {
  echo "=== regenerating config.example.yaml ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/config.example.yaml from src/config.rs struct layout + ~/.config/llmconduit/config.yaml per docs/regenerate.md'"
}

regenerate_upstream() {
  echo "=== regenerating upstream.md ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/upstream.md from src/upstream.rs per docs/regenerate.md'"
}

regenerate_tools() {
  echo "=== regenerating tools.md ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/tools.md from src/search.rs and src/engine.rs per docs/regenerate.md'"
}

regenerate_observability() {
  echo "=== regenerating observability.md ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/observability.md from observability sources per docs/regenerate.md'"
}

regenerate_cli() {
  echo "=== regenerating cli.md ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/cli.md from src/cli.rs + src/main.rs per docs/regenerate.md'"
}

regenerate_dashboard() {
  echo "=== regenerating dashboard.md ==="
  echo "To regenerate, run: claude -p 'Regenerate docs/dashboard.md from dashboard_*.rs sources per docs/regenerate.md'"
}

case "${1:-all}" in
  routes) regenerate_routes ;;
  adapters) regenerate_adapters ;;
  engine) regenerate_engine ;;
  config) regenerate_config ;;
  config.example) regenerate_config_example ;;
  upstream) regenerate_upstream ;;
  tools) regenerate_tools ;;
  observability) regenerate_observability ;;
  cli) regenerate_cli ;;
  dashboard) regenerate_dashboard ;;
  all)
    regenerate_routes
    regenerate_adapters
    regenerate_engine
    regenerate_config
    regenerate_config_example
    regenerate_upstream
    regenerate_tools
    regenerate_observability
    regenerate_cli
    regenerate_dashboard
    echo "=== all done ==="
    ;;
  *) echo "Usage: $0 [routes|adapters|engine|config|config.example|upstream|tools|observability|cli|dashboard|all]" >&2; exit 1 ;;
esac
