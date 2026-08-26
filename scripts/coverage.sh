#!/bin/sh
# Gate line *and* branch coverage, and name the branch arms no test ever took.
#
# `cargo llvm-cov` can fail a build on lines, regions, functions and per-file lines. It cannot
# fail one on branches — there is no `--fail-under-branches` — and branches are the number that
# tells the truth about this suite: they sat twelve points below lines when the bar was first
# measured. This script is the missing gate.
#
# It reads llvm-cov's own report on stdin rather than recomputing anything, so the figures here
# and the ones in the text report are the same figures. Nothing but `sh`, `awk` and `sort` is
# used: a coverage gate that needs its own toolchain installed is one CI skips.
#
#   cargo +nightly llvm-cov report --branch --summary-only | scripts/coverage.sh --min-lines 95
#   cargo +nightly llvm-cov report --branch --text          | scripts/coverage.sh --branches
#
# Three bars, with three different jobs. The **project** bar is the headline and catches aggregate
# regression. The **per-file** bar (`--min-file-lines`) is lower on purpose: its job is that no
# single file rots or lands uncovered, not to push every file to the project number — a 36-line
# file is one uncovered line away from a five-point swing, so a high uniform bar is noise at that
# scale rather than rigour. The **named floors** are where "this one has to be complete" gets said
# explicitly, one reason per entry.
#
# There is deliberately no uniform per-file *branch* bar: the smallest files here carry ten to
# fourteen branches in total, which makes one untaken arm worth seven to ten points.
#
# `--floors` raises the bar on named files past the project-wide one. `cargo-llvm-cov` has
# `--fail-under-file-lines`, but it applies one number to *every* file, which in practice means
# the number the weakest file can pass — the opposite of what a floor on a critical module is
# for. Here each entry is `path=lines[:branches]`, and **a path that is not in the report is an
# error**: a floor whose file was renamed away would otherwise pass forever while measuring
# nothing.
#
# In `--branches` mode the same branch is annotated once per instantiation, so an arm counts as
# untaken only when every copy of it reads zero — which is why the counts are folded together
# before anything is reported. Those totals are a pointer to the work, not the gate: llvm's own
# summary skips folded regions and is the number that decides pass or fail.

set -eu

MIN_LINES=""
MIN_BRANCHES=""
MIN_REGIONS=""
MIN_FUNCTIONS=""
MIN_FILE_LINES=""
FLOORS=""
MODE=check
FILTER=""

usage() {
    cat <<'EOF'
usage: coverage.sh [--min-lines N] [--min-branches N] [--min-regions N] [--min-functions N]
                  [--min-file-lines N] [--floors "path=lines[:branches] ..."]
       coverage.sh --branches [--file SUBSTRING]

Reads `cargo llvm-cov report` output on stdin.

  (no mode)    a per-file table worst-branches-first, then the bars; exit 1 if one is missed
  --min-file-lines  a floor every file must clear on its own, so one bad file cannot hide
               inside a good average. Deliberately below the project bar; see the header.
  --floors     per-file minimums for critical modules, above the project-wide bar. A named
               file that is not in the report is an error, not a pass.
  --branches   every branch arm the suite never took, as file:line:col
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --min-lines) MIN_LINES="$2"; shift 2 ;;
        --min-branches) MIN_BRANCHES="$2"; shift 2 ;;
        --min-regions) MIN_REGIONS="$2"; shift 2 ;;
        --min-functions) MIN_FUNCTIONS="$2"; shift 2 ;;
        --min-file-lines) MIN_FILE_LINES="$2"; shift 2 ;;
        --floors) FLOORS="$2"; shift 2 ;;
        --branches) MODE=branches; shift ;;
        --file) FILTER="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "coverage.sh: unknown argument $1" >&2; usage >&2; exit 2 ;;
    esac
done

report=$(mktemp) || exit 2
trap 'rm -f "$report"' EXIT INT HUP TERM
cat >"$report"

if [ ! -s "$report" ]; then
    echo "coverage.sh: nothing on stdin — did the llvm-cov report fail?" >&2
    exit 2
fi

# ------------------------------------------------------------------ every untaken branch arm

if [ "$MODE" = branches ]; then
    awk -v filter="$FILTER" '
        # `llvm-cov show` heads each file with its absolute path and a colon.
        /^\// && /:$/ { file = substr($0, 1, length($0) - 1); next }

        # |  Branch (163:13): [True: 9, False: 0]
        /Branch \(/ {
            if (file == "") next
            if (filter != "" && index(file, filter) == 0) next

            rest = substr($0, index($0, "Branch (") + 8)
            loc  = substr(rest, 1, index(rest, ")") - 1)
            tail = substr(rest, index(rest, ")"))

            true_count  = substr(tail, index(tail, "True:")  + 5) + 0
            false_count = substr(tail, index(tail, "False:") + 6) + 0

            key = file "|" loc
            seen[key] = 1
            # An arm is taken if *any* instantiation took it.
            if (true_count  > yes[key]) yes[key] = true_count
            if (false_count > no[key])  no[key]  = false_count
        }

        END {
            for (key in seen) {
                if (yes[key] > 0 && no[key] > 0) continue
                split(key, part, "|")
                if (yes[key] == 0 && no[key] == 0) what = "neither arm — the condition never ran"
                else if (yes[key] == 0)            what = "never true"
                else                               what = "never false"
                printf "  %s:%s  %s\n", part[1], part[2], what
                total++
            }
            if (total == 0) print "  every branch arm was taken"
            else printf "\n  %d untaken.\n", total
        }
    ' "$report" | sort
    exit 0
fi

# ------------------------------------------------------------------ the table and the bars
#
# `--summary-only` prints one fixed set of columns per file, and no path in this crate has a
# space in it:
#   1 file  2 regions 3 missed 4 cover  5 funcs 6 missed 7 executed
#   8 lines 9 missed 10 cover  11 branches 12 missed 13 cover
#
# A file with no branches in it prints "-" for the last column. That is not 0%: it has taken
# every branch it has, and scoring it zero would make such a file impossible to pass.

body=$(mktemp) || exit 2
trap 'rm -f "$report" "$body"' EXIT INT HUP TERM

awk '
    NF == 13 && $1 != "Filename" && $1 != "TOTAL" && $1 !~ /^-+$/ {
        branch_cover = ($13 == "-") ? 100 : $13 + 0
        # A sort key first, stripped off again once `sort` has done its work.
        printf "%08.3f|%-32s %7s %5s miss   %7s %5s miss   %8s %9s\n",
            branch_cover, $1, $10, $9, ($13 == "-" ? "n/a" : $13), $12, $4, $7
    }
' "$report" >"$body"

if [ ! -s "$body" ]; then
    echo "coverage.sh: no per-file rows in the report — is it --summary-only output?" >&2
    exit 2
fi

printf '%-32s %7s %10s   %7s %10s   %8s %9s\n' \
    file lines "" branches "" regions functions
printf -- '------------------------------------------------------------------------------------------\n'
sort -n "$body" | cut -d'|' -f2-
printf -- '------------------------------------------------------------------------------------------\n'

# ------------------------------------------------------------------ the per-file floor

floors_failed=0
if [ -n "$MIN_FILE_LINES" ]; then
    awk -v min="$MIN_FILE_LINES" '
        NF == 13 && $1 != "Filename" && $1 != "TOTAL" && $1 !~ /^-+$/ {
            if ($10 + 1e-9 < min + 0) {
                if (!shown++) printf "\nevery file, on its own\n"
                printf "  FAIL  %-30s lines %7.2f%% (>= %s%%)   %s uncovered\n",
                    $1, $10 + 0, min, $9
                failed = 1
            }
        }
        END { exit failed }
    ' "$report" || floors_failed=1
fi

# ------------------------------------------------------------------ the critical-module floors

if [ -n "$FLOORS" ]; then
    printf '\ncritical modules\n'
    awk -v floors="$FLOORS" '
        BEGIN {
            wanted = split(floors, entries, /[ \t]+/)
            for (i = 1; i <= wanted; i++) {
                if (entries[i] == "") continue
                split(entries[i], half, "=")
                split(half[2], bars, ":")
                want_lines[half[1]] = bars[1] + 0
                # Branches are optional: a file with none prints "-" and is not gated on them.
                want_branches[half[1]] = (bars[2] == "") ? "" : bars[2] + 0
                order[++n] = half[1]
            }
        }

        NF == 13 && $1 != "Filename" && $1 != "TOTAL" && $1 !~ /^-+$/ {
            if (!($1 in want_lines)) next
            seen[$1] = 1
            lines[$1] = $10 + 0
            branches[$1] = ($13 == "-") ? "" : $13 + 0
        }

        END {
            for (i = 1; i <= n; i++) {
                file = order[i]
                if (!(file in seen)) {
                    printf "  GONE  %-30s not in the report — renamed, or the floor is stale\n", file
                    failed = 1
                    continue
                }
                bad = (lines[file] + 1e-9 < want_lines[file])
                if (want_branches[file] != "" && branches[file] != "")
                    bad = bad || (branches[file] + 1e-9 < want_branches[file])

                shown = (branches[file] == "") ? "n/a" : sprintf("%.2f%%", branches[file])
                want = (want_branches[file] == "") ? "-" : sprintf("%d%%", want_branches[file])
                printf "  %s  %-30s lines %7.2f%% (>= %d%%)   branches %7s (>= %s)\n",
                    bad ? "FAIL" : "ok  ", file, lines[file], want_lines[file], shown, want
                if (bad) failed = 1
            }
            exit failed
        }
    ' "$report" || floors_failed=1
fi

# ------------------------------------------------------------------ the project-wide bars

printf '\n'
awk -v min_lines="$MIN_LINES" \
    -v min_branches="$MIN_BRANCHES" \
    -v min_regions="$MIN_REGIONS" \
    -v min_functions="$MIN_FUNCTIONS" '
    function bar(name, count, missed, cover,    have, need) {
        if (min[name] == "") return
        have = (cover == "-") ? 100 : cover + 0
        if (have + 1e-9 >= min[name] + 0) {
            printf "  ok    %-10s %6.2f%%  >= %s%%\n", name, have, min[name]
            return
        }
        # How many more have to be covered to clear the bar: the one actionable number.
        need = 0
        while (count > 0 && 100 * (count - missed + need) / count + 1e-9 < min[name] + 0) need++
        printf "  FAIL  %-10s %6.2f%%  <  %s%%   (%d uncovered; %d more to cover)\n",
            name, have, min[name], missed, need
        failed = 1
    }

    $1 == "TOTAL" && NF == 13 {
        seen = 1
        printf "%-32s %7s %5s miss   %7s %5s miss   %8s %9s\n\n",
            "TOTAL", $10, $9, $13, $12, $4, $7

        min["lines"] = min_lines
        min["branches"] = min_branches
        min["regions"] = min_regions
        min["functions"] = min_functions

        bar("lines",     $8 + 0,  $9 + 0,  $10)
        bar("branches",  $11 + 0, $12 + 0, $13)
        bar("regions",   $2 + 0,  $3 + 0,  $4)
        bar("functions", $5 + 0,  $6 + 0,  $7)
    }

    END {
        if (!seen) {
            print "coverage.sh: no TOTAL row in the report" > "/dev/stderr"
            exit 2
        }
        if (failed) exit 1
    }
' "$report" || bars_failed=1

if [ "${bars_failed:-0}" -ne 0 ] || [ "$floors_failed" -ne 0 ]; then
    echo
    echo "coverage is below the bar."
    echo "  make coverage-missing    the lines no test ran"
    echo "  make coverage-branches   the branch arms no test took"
    echo "  make coverage-html       the browsable report"
    exit 1
fi
