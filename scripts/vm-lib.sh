#!/usr/bin/env bash
# F-08 / ADR-010 -- shared plumbing for scripts/vm.sh and scripts/vm-test.sh.
#
# Sourced, never executed. It exists so the two entry points cannot drift on the three
# things they must agree about:
#
#   1. What "a non-interactive ssh call" means (`ssh_nostdin`, which carries `-n`).
#   2. What a guest-authored byte is allowed to do to the console (`relay_guest`).
#   3. Which environment values are allowed to reach a QEMU argument (the numeric checks).
#
# ADR-010's provisioning record claims every non-interactive call in *both* scripts routes
# through `ssh -n`. That claim is only checkable if there is one definition of it; two copies
# would make it a coincidence. The complete set of exceptions, so the claim can be audited by
# grepping for `ssh ` in scripts/:
#
#   * `vm.sh ssh` with no arguments -- an interactive shell, which needs its stdin.
#   * `vm.sh ssh 'cmd'` under `MCP_VM_SSH_STDIN=1` -- the single deliberate opt-in, used by
#     `cmd_probes` to pipe its heredoc in. Without the variable, that path still gets `-n`.
#   * `vm-test.sh`'s `rsync -e` command -- rsync drives its own pipes into that ssh child's
#     stdin, so it is immune to the swallow bug by construction and `-n` would break the
#     transport outright.
#
# Everything else uses `ssh_nostdin` below.

# ---- configuration, with every QEMU-bound numeric validated ------------------------------
VM_DIR="${MCP_VM_DIR:-$HOME/vm/f-08}"
SSH_PORT="${MCP_VM_SSH_PORT:-2222}"
VM_CPUS="${MCP_VM_CPUS:-4}"
VM_MEM="${MCP_VM_MEM:-4096}"
VM_USER="dev"
PIDFILE="$VM_DIR/qemu.pid"
KEY="$VM_DIR/ssh/id_ed25519"
MAX_GUEST_BYTES="${MCP_VM_MAX_GUEST_BYTES:-262144}"

# Host-authored lines carry the `==>` sigil; `==> [host] ` is a verdict the host computed
# itself. relay_guest() prefixes every guest line with `  [guest] ` *host-side* and strips CR,
# so a guest cannot return to column 0 and cannot forge either host slot.
say()  { printf '==> %s\n' "$*"; }
hsay() { printf '==> [host] %s\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# These values land inside QEMU's comma-separated option strings, where a value carrying a
# comma is parsed as a second *option*, not as a bad one. Demonstrated while hardening this
# script: MCP_VM_SSH_PORT='2222-:22,hostfwd=tcp::12345' builds
#   -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2222-:22,hostfwd=tcp::12345-:22
# which QEMU accepts as two forwards, the second bound to 0.0.0.0 -- i.e. the guest's sshd
# published off-host, from one environment variable. A digits-only check closes it totally.
vm_validate_numerics() {
  [[ "$SSH_PORT" =~ ^[0-9]{1,5}$ ]] && (( SSH_PORT >= 1 && SSH_PORT <= 65535 )) \
    || die "MCP_VM_SSH_PORT must be a plain integer 1-65535, got: ${SSH_PORT@Q}"
  [[ "$VM_CPUS" =~ ^[0-9]{1,3}$ ]] && (( VM_CPUS >= 1 )) \
    || die "MCP_VM_CPUS must be a plain positive integer, got: ${VM_CPUS@Q}"
  [[ "$VM_MEM" =~ ^[0-9]{1,7}$ ]] && (( VM_MEM >= 256 )) \
    || die "MCP_VM_MEM must be a plain integer of MiB, >= 256, got: ${VM_MEM@Q}"
  [[ "$MAX_GUEST_BYTES" =~ ^[0-9]{1,10}$ ]] && (( MAX_GUEST_BYTES >= 1024 )) \
    || die "MCP_VM_MAX_GUEST_BYTES must be a plain integer >= 1024, got: ${MAX_GUEST_BYTES@Q}"
}
vm_validate_numerics

# ---- ssh ---------------------------------------------------------------------------------
ssh_opts=(
  -i "$KEY"
  -p "$SSH_PORT"
  -o StrictHostKeyChecking=no
  # The guest is recreated from a pinned image whenever it is reprovisioned, so its host key
  # legitimately changes; a persistent known_hosts entry would only ever be a false alarm
  # here. This is sound only while nothing hostile runs in or near this guest -- see the
  # "Preconditions" list on F-08 in docs/tasks.md, which records pinning a real known_hosts
  # as a precondition on the transition to untrusted execution, with the precise limit of
  # what an impersonator on 127.0.0.1:2222 would gain.
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
  -o ConnectTimeout=5
)

# Internal, non-interactive calls: `-n` detaches stdin. Without it, an ssh invoked from a
# script that is itself being fed on stdin (`bash -s < script`, which is how a
# non-interactive agent or a CI step naturally drives these) consumes the remainder of that
# script, and the caller then silently stops executing with a ZERO exit status -- a
# green-looking no-op, the worst shape a harness script can fail in. Found the hard way in
# vm.sh, then found again in vm-test.sh's own reachability probe and `run_remote` after the
# vm.sh fix had already been written up as complete. Hence this one shared definition.
ssh_nostdin=("${ssh_opts[@]}" -n -o BatchMode=yes)

# ---- guest output ------------------------------------------------------------------------
# Everything a guest writes lands on a console that, in this project's workflow, is read by a
# model: the same output-surface trust problem P0-11 fixed for the Stage 2 census
# (`crates/discovery/src/transport.rs::relay_child_stderr`). Unbounded volume, plus raw
# ANSI/OSC sequences that clear scrollback, repaint a forged transcript, set the terminal
# title, or reach the clipboard via OSC 52. Demonstrated here before fixing: a guest emitting
# `ESC[2J`, SGR colour, an OSC title-set and an alt-screen switch had every escape byte
# arrive intact host-side. Same three properties as the Rust drain:
#
#   * Bounded  -- MCP_VM_MAX_GUEST_BYTES of relayed output, then one truncation notice.
#                 Reading continues past the cap and discards, because a guest whose pipe
#                 fills blocks on write and a blocked guest looks like a hung VM. `fold`
#                 bounds record length too, so one newline-free gigabyte cannot grow awk's
#                 record buffer on the host.
#   * Stripped -- C0 except LF and TAB, plus CR, DEL and every byte >= 0x80 (which covers the
#                 C1 CSI/OSC introducers 0x9b/0x9d). Dropped, never substituted, so nothing
#                 is invented; a line that was pure escapes relays as empty. The cost,
#                 disclosed rather than discovered later: legitimate non-ASCII is dropped too,
#                 so `cargo purity`'s own "OK — [...]" em-dash arrives as "OK  [...]". Same
#                 trade the Rust drain makes, for the same reason -- a whitelist of printable
#                 ASCII is checkable at a glance, whereas "all of UTF-8 except the dangerous
#                 parts" is not.
#   * Marked   -- prefixed host-side, so provenance is visible where it lands.
#
# `stdbuf -oL` keeps it streaming: without it tr and fold block-buffer and a long gate would
# print nothing at all until it finished.
#
# This is NOT a trust boundary. ssh's exit status *is* the remote exit status, so guest output
# and guest exit code are one guest-controlled channel; see precondition 4 on F-08. What this
# buys is that a guest cannot forge the host's own lines and cannot own the scrollback.
relay_guest() {
  LC_ALL=C stdbuf -oL tr -d '\000-\010\013-\037\177-\377' \
    | LC_ALL=C stdbuf -oL fold -b -w 4000 \
    | LC_ALL=C awk -v max="$MAX_GUEST_BYTES" '
        over { next }
        { n += length($0) + 1
          if (n > max) {
            over = 1
            print "  [guest] ... further guest output suppressed after " max " relayed bytes"
            fflush()
            next
          }
          print "  [guest] " $0
          fflush()
        }'
}

# Run one command in the guest, relaying its output through relay_guest and returning the
# GUEST's exit status (PIPESTATUS[0]), not the relay's. errexit is suspended across the
# pipeline deliberately: `cmd | relay || true` would reset PIPESTATUS to (0) and lose the
# status entirely, which is exactly the class of silent-success bug this file exists to stop.
vm_ssh_relay() {
  local rc
  set +e
  ssh "${ssh_nostdin[@]}" "$VM_USER@127.0.0.1" "$@" 2>&1 | relay_guest
  rc=${PIPESTATUS[0]}
  set -e
  return "$rc"
}

# ---- liveness ----------------------------------------------------------------------------
# A pidfile plus `kill -0` cannot tell the VM from a recycled PID, so the process name is
# checked too ("qemu-system-x86" after the kernel's 15-char comm truncation). This matters for
# `stop`, which must never delete a pidfile it has not proven dead, and for `start`, which
# would otherwise launch a second QEMU against the same qcow2.
running() {
  [[ -f "$PIDFILE" ]] || return 1
  local pid
  pid="$(cat "$PIDFILE" 2>/dev/null)" || return 1
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  [[ -r "/proc/$pid/comm" ]] || return 1
  grep -q qemu "/proc/$pid/comm"
}
