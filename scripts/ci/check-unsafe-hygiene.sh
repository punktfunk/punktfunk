#!/bin/sh
# Unsafe-hygiene grep gates (rust-safety programme §4 WP2c). Three classes no lint covers:
#
#   A. `unsafe fn` whose body contains no unsafe operation. Because the workspaces deny
#      `unsafe_op_in_unsafe_fn`, every real unsafe op inside an `unsafe fn` sits in an explicit
#      `unsafe {}` block — so an `unsafe fn` with no `unsafe` in its body is a marker carrying no
#      contract (db659809 found two by hand, with call-site SAFETY proofs describing FFI the fns
#      no longer performed). A contract-DEFERRING fn (`set_len` shape: safe body, the danger is in
#      later safe code trusting the argument) is legitimate — waive it with a comment line
#      `// unsafe-fn-no-op-ok: <why the marker still carries a contract>` above the fn (doc lines
#      may sit between). Two classes are skipped structurally: files carrying
#      `#![allow(unsafe_op_in_unsafe_fn)]` (the fenced GPU/FFI backends — there the premise that
#      ops are forced into blocks does not hold), and `unsafe extern "ABI" fn` definitions (loader
#      / framework callbacks, where the unsafe marker is dictated by the PFN type they must match,
#      not by a caller contract; gate B still covers their bodies).
#
#   B. `unwrap`/`expect`/`panic!` inside an `extern "C"` / `extern "system"` fn body. Panic across
#      an `extern` boundary is an abort since Rust 1.81 — not a diagnostic, not a sanitizer
#      finding, not fuzzable (8b98d0b3: an ETW callback's `RING.lock().unwrap()` aborted the host
#      on a poisoned lock). A body that routes through `catch_unwind` (the C ABI's pattern) is
#      exempt; otherwise waive a deliberate abort with `// panic-in-extern-ok: <reason>` directly
#      above the fn.
#
#   C. Safe-but-process-global APIs: `env::set_var`/`remove_var`, `sigaction`, `setlocale`,
#      `set_current_dir`. Each is (or was) callable without `unsafe` and unsound (or racy) from a
#      live multithreaded process — the 972af299 environ data race lived in a file with ZERO
#      occurrences of the word `unsafe`, invisible to the census. Since the edition-2024
#      migration the env pair is `unsafe fn` (compiler-enforced, SAFETY proof per site, counted
#      by the census); this ratchet stays for the still-safe APIs (`sigaction`, `setlocale`,
#      `set_current_dir`) and as a growth brake on env mutation generally. The baseline below
#      enumerates today's debt per file; ANY increase (or a new file) fails. Shrink a file's
#      count? Lower its baseline in the same commit.
#
# All three gates were shown to FAIL on deliberately planted instances before being made blocking
# (the gate-of-the-gate rule that caught cd72f77a's `0 * SLOT`).
#
# Textual gates, so textual limits: string literals containing `unsafe {` and macro-generated fns
# are invisible; nested `unsafe fn` items inside another fn's body attribute their blocks to the
# outer fn. Both classes are rare here and covered by review.

set -u
cd "$(dirname "$0")/../.." || exit 2

fail=0
tmp="${TMPDIR:-/tmp}/unsafe-hygiene.$$"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT

# Tracked, non-vendored Rust sources.
git ls-files '*.rs' | grep -v '/vendor/' > "$tmp/files"

# ---------------------------------------------------------------- gate A
awk '
function reset() { state = 0; has_unsafe = 0; depth = 0 }
FNR == 1 { reset(); fenced = 0; waive_next = 0 }
/^#!\[allow\(unsafe_op_in_unsafe_fn\)\]$/ { fenced = 1 }
{
    line = $0
    sub(/^[ \t]+/, "", line)
    is_comment = (line ~ /^\/\//)
    if (is_comment && line ~ /unsafe-fn-no-op-ok:/) { waive_next = 1 }
}
fenced { next }
# fn-definition start: a plain `unsafe fn` outside a comment — not a type alias, not an
# `unsafe extern "ABI" fn` (signature-mandated markers; see the header)
state == 0 && !is_comment && /(^|[ \t(])unsafe[ \t]+fn[ \t]+[A-Za-z_]/ \
    && $0 !~ /^[ \t]*type[ \t]/ && $0 !~ /=[ \t]*unsafe/ {
    state = 1; sig_file = FILENAME; sig_line = FNR
    name = $0; sub(/.*fn[ \t]+/, "", name); sub(/[^A-Za-z0-9_].*/, "", name)
    waived = waive_next
}
state == 1 {
    # declaration (trait method / extern block) ends before a body opens
    if ($0 ~ /;/ && $0 !~ /{/) { reset(); next }
    if ($0 ~ /{/) {
        state = 2
        # count braces via gsub (returns the count, leaves the line unchanged) — the
        # empty-separator split() alternative is a gawk extension mawk lacks
        t = $0; opens = gsub(/\{/, "{", t); t = $0; closes = gsub(/\}/, "}", t)
        depth = opens - closes
        if (opens > 0 && depth == 0) {   # one-line body
            if ($0 ~ /unsafe[ \t]*{[^}]*}[^}]*}/ || $0 ~ /unsafe impl/) has_unsafe = 1
            if (!has_unsafe && !waived) { print sig_file ":" sig_line ": unsafe fn `" name "` has no unsafe operation in its body"; bad = 1 }
            reset()
        }
        next
    }
    next
}
state == 2 {
    if (!is_comment && ($0 ~ /unsafe[ \t]*{/ || $0 ~ /unsafe impl/)) has_unsafe = 1
    t = $0; opens = gsub(/\{/, "{", t); t = $0; closes = gsub(/\}/, "}", t)
    depth += opens - closes
    if (depth <= 0) {
        if (!has_unsafe && !waived) { print sig_file ":" sig_line ": unsafe fn `" name "` has no unsafe operation in its body"; bad = 1 }
        reset()
    }
}
!is_comment { waive_next = 0 }
END { exit bad ? 1 : 0 }
' $(cat "$tmp/files") > "$tmp/gate_a" 2>&1
if [ -s "$tmp/gate_a" ]; then
    echo "GATE A — unsafe fn markers carrying no contract (waive a contract-deferring fn with"
    echo "         '// unsafe-fn-no-op-ok: <reason>' on the line above):"
    cat "$tmp/gate_a"
    fail=1
fi

# ---------------------------------------------------------------- gate B
awk '
function reset() { state = 0; depth = 0; guarded = 0; nhit = 0 }
FNR == 1 { reset(); waive_next = 0 }
{
    line = $0
    sub(/^[ \t]+/, "", line)
    is_comment = (line ~ /^\/\//)
    if (is_comment && line ~ /panic-in-extern-ok:/) { waive_next = 1 }
}
state == 0 && !is_comment && /extern[ \t]+"(C|system)"[ \t]+fn[ \t]+[A-Za-z_]/ \
    && $0 !~ /^[ \t]*type[ \t]/ && $0 !~ /=[ \t]*(unsafe[ \t]+)?extern/ {
    state = 1; sig_file = FILENAME; sig_line = FNR
    name = $0; sub(/.*fn[ \t]+/, "", name); sub(/[^A-Za-z0-9_].*/, "", name)
    waived = waive_next
}
state == 1 {
    if ($0 ~ /;/ && $0 !~ /{/) { reset(); next }
    if ($0 ~ /{/) {
        state = 2
        t = $0; opens = gsub(/\{/, "{", t); t = $0; closes = gsub(/\}/, "}", t)
        depth = opens - closes
        if (depth == 0) reset()
        next
    }
    next
}
state == 2 {
    if ($0 ~ /catch_unwind/) guarded = 1
    if (!is_comment && ($0 ~ /\.unwrap\(\)/ || $0 ~ /\.expect\(/ || $0 ~ /(^|[^a-zA-Z0-9_])panic!/)) {
        nhit++; hitline[nhit] = FILENAME ":" FNR
    }
    t = $0; opens = gsub(/\{/, "{", t); t = $0; closes = gsub(/\}/, "}", t)
    depth += opens - closes
    if (depth <= 0) {
        if (nhit > 0 && !guarded && !waived) {
            for (i = 1; i <= nhit; i++)
                print hitline[i] ": unwrap/expect/panic! reachable in extern fn `" name "` (no catch_unwind)"
            bad = 1
        }
        reset()
    }
}
!is_comment { waive_next = 0 }
END { exit bad ? 1 : 0 }
' $(cat "$tmp/files") > "$tmp/gate_b" 2>&1
if [ -s "$tmp/gate_b" ]; then
    echo "GATE B — panic across an extern boundary aborts the process since Rust 1.81. Route the"
    echo "         body through catch_unwind (see punktfunk-ffi) or waive a deliberate"
    echo "         abort with '// panic-in-extern-ok: <reason>' on the line above the fn:"
    cat "$tmp/gate_b"
    fail=1
fi

# ---------------------------------------------------------------- gate C
# Baseline: per-file count of process-global-API mentions (call sites AND comments — the grep is
# the contract; keep it dumb and stable). Regenerate a line with:
#   grep -c 'env::set_var\|env::remove_var\|sigaction\|setlocale\|set_current_dir' <file>
# A startup scrub of our OWN credentials is the one shape that belongs here rather than at a
# spawn site: Command::env_remove leaves them in /proc/self/environ for every same-uid reader.
cat > "$tmp/gate_c_baseline" <<'BASELINE'
clients/linux/src/app/mod.rs:1
clients/linux/src/app/spawn.rs:1
clients/session/src/main.rs:4
crates/pf-console-ui/src/screens/settings/tests.rs:1
crates/pf-encode-win/src/windows/nvenc.rs:4
crates/pf-encode/src/enc/linux/nvenc_cuda.rs:2
crates/pf-encode/src/enc/linux/worker.rs:1
crates/pf-inject/src/inject/linux/steam_gadget.rs:5
crates/pf-vdisplay/src/lib.rs:1
crates/pf-vdisplay/src/vdisplay/routing.rs:4
crates/pf-vdisplay/src/vdisplay/session.rs:10
crates/pf-vkdecode/tests/common/mod.rs:1
crates/pf-vkdecode/tests/gpu_parity.rs:2
crates/pf-win-display/src/win_display.rs:2
crates/punktfunk-core/src/quic/endpoint.rs:2
crates/punktfunk-host/src/hostsys/identity.rs:3
crates/punktfunk-host/src/library/art.rs:2
crates/punktfunk-host/src/main.rs:1
crates/punktfunk-host/src/mgmt/tests.rs:3
crates/punktfunk-host/src/native.rs:2
crates/punktfunk-host/src/windows/service/host_env.rs:1
packaging/windows/drivers/pf-vdisplay/src/encode/thread.rs:1
BASELINE

: > "$tmp/gate_c"
while IFS= read -r f; do
    n=$(grep -c 'env::set_var\|env::remove_var\|sigaction\|setlocale\|set_current_dir' "$f")
    [ "$n" -eq 0 ] && continue
    base=$(grep -F "$f:" "$tmp/gate_c_baseline" | head -1 | awk -F: '{print $NF}')
    base=${base:-0}
    if [ "$n" -gt "$base" ]; then
        echo "$f: $n process-global-API mentions (baseline $base)" >> "$tmp/gate_c"
    fi
done < "$tmp/files"
if [ -s "$tmp/gate_c" ]; then
    echo "GATE C — env::set_var/remove_var, sigaction, setlocale, set_current_dir are safe to"
    echo "         call and unsound from a live multithreaded process (972af299). Fix the new"
    echo "         call site (a per-call env override belongs in Command::env; a handler install"
    echo "         belongs behind Once at startup) rather than raising the baseline:"
    cat "$tmp/gate_c"
    fail=1
fi

if [ "$fail" -eq 0 ]; then
    echo "unsafe-hygiene: all three gates clean"
fi
exit "$fail"
