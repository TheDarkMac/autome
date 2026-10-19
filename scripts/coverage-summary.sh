#!/usr/bin/env bash
# Tableau Markdown de la couverture de code (#247), pour le résumé d'un job
# GitHub Actions (`>> "$GITHUB_STEP_SUMMARY"`) ou la console.
#
#   scripts/coverage-summary.sh rust   <export JSON de cargo llvm-cov>
#   scripts/coverage-summary.sh vitest <coverage-summary.json de Vitest>
#
# rust   : `cargo llvm-cov report --json --summary-only` ; total puis une
#          ligne par crate (fichiers regroupés par `crates/<nom>/`).
# vitest : reporter `json-summary` (total seulement).
#
# Prérequis : `jq`.
set -euo pipefail

usage() { echo "usage : $0 rust|vitest <fichier.json>" >&2; exit 2; }
[[ $# -eq 2 ]] || usage
kind="$1"
file="$2"
[[ -r "$file" ]] || { echo "fichier illisible : $file" >&2; exit 1; }

case "$kind" in
  rust)
    jq -r '
      # Tronqué au dixième (jamais arrondi au-dessus du plancher), « 95.0 ».
      def pct($c; $n):
        if $n == 0 then "—"
        else ($c * 1000 / $n | floor) as $p | "\($p / 10 | floor).\($p % 10) %"
        end;
      def row($name; $l; $f; $r):
        "| \($name) | \(pct($l.covered; $l.count)) (\($l.covered)/\($l.count)) | \(pct($f.covered; $f.count)) (\($f.covered)/\($f.count)) | \(pct($r.covered; $r.count)) (\($r.covered)/\($r.count)) |";
      def sum($k): { count: (map(.summary[$k].count) | add), covered: (map(.summary[$k].covered) | add) };
      .data[0] as $d
      | "### Couverture backend (cargo llvm-cov)",
        "",
        "| Périmètre | Lignes | Fonctions | Régions |",
        "|---|---|---|---|",
        row("**Total**"; $d.totals.lines; $d.totals.functions; $d.totals.regions),
        ( $d.files
          | map(. + { crate: (.filename | capture("crates/(?<c>[^/]+)/").c // "autres") })
          | group_by(.crate)[]
          | row("`\(.[0].crate)`"; sum("lines"); sum("functions"); sum("regions")) )
    ' "$file"
    ;;
  vitest)
    jq -r '
      def cell($m): "\($m.pct) % (\($m.covered)/\($m.total))";
      .total as $t
      | "### Couverture frontend (Vitest, v8)",
        "",
        "| Lignes | Instructions | Fonctions | Branches |",
        "|---|---|---|---|",
        "| \(cell($t.lines)) | \(cell($t.statements)) | \(cell($t.functions)) | \(cell($t.branches)) |"
    ' "$file"
    ;;
  *) usage ;;
esac
