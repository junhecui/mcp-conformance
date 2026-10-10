#!/usr/bin/env bash
# F-08 / ADR-010 -- run the workspace's four gates inside the pinned VM.
#
#   scripts/vm-test.sh              sync this worktree and run all four gates
#   scripts/vm-test.sh build test   run a subset, in the order given
#
# Gates: build | test | clippy | purity -- the same four HANDOFF.md §3.1 reports for WSL2
# and macOS, so a result here is directly comparable to those.
#
# Why bash, and why rsync-over-SSH rather than a shared mount:
#   * The hypervisor runs inside WSL2 (ADR-010's Windows provisioning record), so the whole
#     loop is native Linux; a PowerShell script would only shell back into WSL.
#   * A 9p/virtiofs/drvfs share of the repo is the one thing this project must not do
#     casually: HANDOFF.md §3 flags CRLF conversion of byte-exact fixtures as a live hazard
#     (there is a CRLF-contaminated clone on this very host), and `evtree`/`world` assert
#     byte-identical captures. rsync over SSH moves bytes verbatim and shares no filesystem
#     semantics with the host at all.
#   * CARGO_TARGET_DIR stays VM-local (/home/dev/cargo-target, set by the guest's
#     /etc/profile.d/99-mcp-conformance.sh) and is never synced or shared. Asserted below,
#     not merely printed.
#
# ssh plumbing, the guest-output relay, and the MCP_VM_* validation all live in
# scripts/vm-lib.sh, shared with scripts/vm.sh so the two cannot drift.
set -euo pipefail

# shellcheck source=scripts/vm-lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/vm-lib.sh"

REMOTE_DIR="/home/dev/mcp-conformance"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

[[ -f "$KEY" ]] || die "no key at $KEY -- run scripts/vm-provision.sh"

# `rsync -e` splits its value on whitespace with no quoting of any kind, so unlike vm.sh's
# array form it cannot carry a path containing a space. Fail loudly instead of silently
# rsyncing with a mangled ssh command line.
[[ "$KEY" != *[[:space:]]* ]] \
  || die "MCP_VM_DIR must not contain whitespace: rsync -e cannot quote \"$KEY\""

# Assert the VM this script manages is actually the process on the far end of the forward,
# before handing it the whole worktree. The reachability probe below only proves *something*
# answers on 127.0.0.1:$SSH_PORT with our key; this proves our QEMU is alive. (Neither is a
# substitute for a pinned known_hosts -- see precondition 8 on F-08 in docs/tasks.md.)
running || die "the VM is not running (no live QEMU for $PIDFILE) -- scripts/vm.sh start"

# `-n` is not optional: without it this ssh eats the rest of a caller that is itself being fed
# on stdin, and the caller then stops executing with a zero exit status. `ssh_nostdin` carries
# it; see the comment on that array in scripts/vm-lib.sh.
ssh "${ssh_nostdin[@]}" "$VM_USER@127.0.0.1" true 2>/dev/null \
  || die "VM unreachable on 127.0.0.1:$SSH_PORT -- scripts/vm.sh start && scripts/vm.sh wait"

say "syncing $REPO_ROOT -> $VM_USER@vm:$REMOTE_DIR (rsync over ssh)"
# --delete so the guest tree is an exact mirror: a file deleted locally must not linger and
# silently keep compiling. target/ is excluded in both directions -- host artifacts are the
# wrong architecture-and-toolchain mix to be reused, and the guest's live target dir is
# CARGO_TARGET_DIR, outside this tree entirely.
#
# `rsync -a` does not read .gitignore, so it carries untracked working-directory files too.
# Two of those must not cross into a machine that will eventually host hostile code:
#   * `.env*` and `/config.local.*` are the paths .gitignore already names as expected-secret;
#     the moment one exists it would otherwise be copied into the guest.
#   * `/.git/` is pointless here (in a worktree it is a *file* pointing at the main repo's
#     .git/worktrees/..., so the guest's copy is a dangling pointer) and is repo history
#     needlessly inside the blast radius. Nothing in the build reads it: there is no build.rs
#     and no vergen/git2 anywhere in the workspace.
# Excluded paths are also protected from --delete, so a previously-synced copy stays until
# removed by hand; that is rsync's behaviour, noted so it is not a surprise.
#
# The rsh command deliberately does NOT carry `-n`: rsync drives its own pipes into that ssh
# child's stdin, so it is immune to the stdin-swallow bug by construction, and `-n` would
# break the transport outright. The array is expanded to a string only here, where rsync's
# interface demands one.
rsync_rsh=(ssh "${ssh_opts[@]}" -o BatchMode=yes)
rsync -a --delete \
  --exclude '/target/' --exclude '/**/target/' \
  --exclude '/.git/' --exclude '/.git' \
  --exclude '.env' --exclude '.env.*' --exclude '/config.local.*' \
  -e "${rsync_rsh[*]}" \
  "$REPO_ROOT/" "$VM_USER@127.0.0.1:$REMOTE_DIR/"

# `bash -lc` so /etc/profile.d/99-mcp-conformance.sh is sourced and CARGO_TARGET_DIR is set;
# a non-login non-interactive ssh command would get neither. Output is relayed through
# relay_guest (bounded, control-character-stripped, `  [guest] `-prefixed) and the GUEST's
# exit status is what comes back -- see scripts/vm-lib.sh.
run_remote() { vm_ssh_relay "bash -lc 'cd $REMOTE_DIR && $1'"; }

gate_build()  { run_remote 'cargo build --workspace'; }
gate_test()   { run_remote 'cargo test --workspace'; }
gate_clippy() { run_remote 'cargo clippy --workspace --all-targets -- -D warnings'; }
gate_purity() { run_remote 'cargo purity'; }

gates=("$@"); [[ ${#gates[@]} -eq 0 ]] && gates=(build test clippy purity)
for g in "${gates[@]}"; do
  declare -F "gate_$g" >/dev/null || die "unknown gate: $g (want: build test clippy purity)"
done

say "environment"
run_remote 'echo "kernel:   $(uname -r)"; echo "rustc:    $(rustc --version)"; echo "cargo:    $(cargo --version)"; echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-<unset!>}"' || true

# Assert CARGO_TARGET_DIR rather than only printing it. If
# /etc/profile.d/99-mcp-conformance.sh ever went missing, cargo would fall back to the synced
# tree's ./target -- which rsync both excludes AND (being excluded) protects from --delete,
# i.e. exactly the stale-artifact state this sync model exists to prevent -- and every gate
# would still print PASS. Three distinct exit codes so the failure says which rule it broke.
say "asserting the guest's CARGO_TARGET_DIR"
if run_remote 'test -n "${CARGO_TARGET_DIR:-}" || exit 11; case "$CARGO_TARGET_DIR" in /*) ;; *) exit 12 ;; esac; case "$CARGO_TARGET_DIR" in '"$REMOTE_DIR"'/*) exit 13 ;; esac; echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR is absolute and outside the synced tree"'; then
  hsay "CARGO_TARGET_DIR: OK"
else
  rc=$?
  case "$rc" in
    11) die "guest CARGO_TARGET_DIR is unset -- check /etc/profile.d/99-mcp-conformance.sh" ;;
    12) die "guest CARGO_TARGET_DIR is not an absolute path" ;;
    13) die "guest CARGO_TARGET_DIR is inside $REMOTE_DIR -- it must not be in the synced tree" ;;
    *)  die "could not read the guest's CARGO_TARGET_DIR (ssh/exit $rc)" ;;
  esac
fi

declare -a results=()
status=0
for g in "${gates[@]}"; do
  say "gate: $g"
  # Every PASS/FAIL below is the host's own, derived from the gate's exit status; the `[host]`
  # sigil is a slot relayed guest output cannot occupy, so a guest that prints its own "PASS
  # test" cannot be mistaken for this line. It is not an independent oracle: ssh's exit status
  # IS the remote exit status, so a guest that has run untrusted code can produce both -- see
  # precondition 4 on F-08 in docs/tasks.md.
  if "gate_$g"; then results+=("PASS  $g"); hsay "gate $g: PASS"
  else results+=("FAIL  $g"); hsay "gate $g: FAIL"; status=1; fi
done

say "summary (inside the pinned VM; host-side verdicts)"
printf '==> [host] %s\n' "${results[@]}"
exit $status
