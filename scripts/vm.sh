#!/usr/bin/env bash
# F-08 / ADR-010 -- lifecycle for the pinned Linux VM: start | stop | status | ssh | wait.
#
#   scripts/vm-provision.sh      build the image/seed/disk once (idempotent)
#   scripts/vm.sh start          boot it, detached
#   scripts/vm.sh wait           block until cloud-init has finished provisioning
#   scripts/vm.sh ssh [cmd...]   shell in, or run one command
#   scripts/vm.sh status         pid, uptime, ssh reachability
#   scripts/vm.sh probes         re-run F-00's four kernel/cgroup/ns/overlay probes
#   scripts/vm.sh stop           graceful poweroff, then SIGTERM, then SIGKILL -- verified
#
# Why bash and not PowerShell: the hypervisor runs inside WSL2 (ADR-010's Windows
# provisioning record explains why), so there is no Windows-side process to manage.
#
# Environment: MCP_VM_DIR, MCP_VM_SSH_PORT, MCP_VM_CPUS, MCP_VM_MEM, MCP_VM_RESTRICT,
# MCP_VM_MAX_GUEST_BYTES, MCP_VM_SSH_STDIN. Everything that reaches a QEMU argument is
# validated in scripts/vm-lib.sh, which also owns the `ssh -n` default and the guest-output
# relay that both this script and scripts/vm-test.sh use.
set -euo pipefail

# shellcheck source=scripts/vm-lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/vm-lib.sh"

cmd_start() {
  running && { say "already running (pid $(cat "$PIDFILE"))"; return 0; }
  [[ -f "$VM_DIR/disk.qcow2" && -f "$VM_DIR/seed.iso" ]] \
    || die "missing artifacts in $VM_DIR -- run scripts/vm-provision.sh first"
  [[ -r /dev/kvm ]] || die "/dev/kvm not readable; without it QEMU falls back to TCG emulation"
  # A stale pidfile is removed only after `running()` has said no -- and `running()` checks
  # the process name, not just `kill -0`, so a recycled PID cannot be mistaken for the VM.
  # QEMU also takes its own write lock on the qcow2, so a second instance against the same
  # disk fails loudly rather than corrupting it.
  rm -f "$PIDFILE"

  # `-netdev user` has no `restrict=` by default, which provisioning requires (the cloud-init
  # apt and rustup steps need egress) and which also means the guest reaches this WSL distro's
  # loopback and the internet. MCP_VM_RESTRICT=1 adds `restrict=on`, cutting the guest off
  # from everything except the hostfwd above -- an opt-in, so the network-isolation
  # precondition recorded on F-08 in docs/tasks.md is actionable rather than theoretical. It
  # is deliberately not the default: an unprovisioned guest cannot provision itself under it.
  local netdev="user,id=n0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22"
  if [[ "${MCP_VM_RESTRICT:-0}" == 1 ]]; then
    netdev="${netdev},restrict=on"
    say "MCP_VM_RESTRICT=1: netdev restrict=on (no egress, no host loopback; ssh forward only)"
  fi

  say "booting (kvm, ${VM_CPUS} vCPU, ${VM_MEM} MiB, ssh on 127.0.0.1:${SSH_PORT})"
  qemu-system-x86_64 \
    -name mcp-conformance-f08 \
    -machine q35,accel=kvm \
    -cpu host \
    -smp "$VM_CPUS" \
    -m "$VM_MEM" \
    -drive "file=$VM_DIR/disk.qcow2,if=virtio,format=qcow2,cache=writeback,discard=unmap" \
    -drive "file=$VM_DIR/seed.iso,if=virtio,format=raw,readonly=on" \
    -device virtio-rng-pci \
    -netdev "$netdev" \
    -device virtio-net-pci,netdev=n0 \
    -display none \
    -serial "file:$VM_DIR/serial.log" \
    -monitor none \
    -pidfile "$PIDFILE" \
    -daemonize
  say "pid $(cat "$PIDFILE"); console log: $VM_DIR/serial.log"
}

cmd_wait() {
  running || die "not running -- scripts/vm.sh start"
  say "waiting for sshd"
  for _ in $(seq 1 120); do
    ssh "${ssh_nostdin[@]}" "$VM_USER@127.0.0.1" true 2>/dev/null && break
    sleep 5
  done
  ssh "${ssh_nostdin[@]}" "$VM_USER@127.0.0.1" true 2>/dev/null \
    || die "sshd never came up; see $VM_DIR/serial.log"
  # sshd is up long before the toolchain exists, so wait on cloud-init's own completion plus
  # the sentinel that user-data writes last.
  say "waiting for cloud-init to finish provisioning"
  for _ in $(seq 1 120); do
    if ssh "${ssh_nostdin[@]}" "$VM_USER@127.0.0.1" \
         'test -f /var/lib/cloud/mcp-provisioned' 2>/dev/null; then
      hsay "provisioned"
      vm_ssh_relay 'cloud-init status' || true
      return 0
    fi
    sleep 10
  done
  die "cloud-init did not finish in time; check: scripts/vm.sh ssh 'cloud-init status --long'"
}

# The deliberate raw escape hatch: output is NOT passed through relay_guest here, because this
# is the one entry point whose job is to hand a human -- or a caller capturing bytes -- the
# guest's stream verbatim, and an interactive shell needs its pty and its escape sequences.
# Everything the harness itself consumes goes through vm_ssh_relay instead.
cmd_ssh() {
  running || die "not running -- scripts/vm.sh start"
  # `-n` by default, so `vm.sh ssh 'cmd'` is safe to call from another script; set
  # MCP_VM_SSH_STDIN=1 to deliberately pipe data in (cmd_probes is the one such site).
  if [[ $# -gt 0 ]]; then
    if [[ "${MCP_VM_SSH_STDIN:-0}" == 1 ]]; then ssh "${ssh_opts[@]}" "$VM_USER@127.0.0.1" "$@"
    else ssh "${ssh_opts[@]}" -n "$VM_USER@127.0.0.1" "$@"; fi
  else ssh "${ssh_opts[@]}" "$VM_USER@127.0.0.1"; fi
}

cmd_status() {
  if running; then
    say "running, pid $(cat "$PIDFILE")"
    if ssh "${ssh_nostdin[@]}" "$VM_USER@127.0.0.1" true 2>/dev/null; then
      hsay "ssh OK on 127.0.0.1:${SSH_PORT}"
      vm_ssh_relay 'uname -r; uptime' || true
    else
      hsay "ssh not reachable yet"
    fi
  else
    hsay "not running"
  fi
}

# Teardown escalates and then VERIFIES. The previous version sent one SIGTERM, waited, and
# then removed the pidfile and printed "stopped" unconditionally -- so a guest that ignored
# the poweroff left a live QEMU that `running()` could no longer see, after which `start`
# would launch a second instance against the same qcow2. That is the same failure class as
# P0-06's orphaned containers, and a teardown path that claims clean teardown without
# checking is what ADR-004's integrity gate forbids one level down. The pidfile is now
# removed only after the process has been observed gone, and an undead QEMU is a `die`
# rather than a success.
cmd_stop() {
  running || { hsay "not running"; return 0; }
  local pid; pid="$(cat "$PIDFILE")"
  say "shutting down (pid $pid)"
  ssh "${ssh_nostdin[@]}" "$VM_USER@127.0.0.1" 'sudo systemctl poweroff' 2>/dev/null || true
  _wait_gone "$pid" 30 2 \
    && { rm -f "$PIDFILE"; hsay "stopped (graceful poweroff; pid $pid verified gone)"; return 0; }

  say "graceful shutdown timed out; SIGTERM"
  kill -TERM "$pid" 2>/dev/null || true
  _wait_gone "$pid" 15 1 \
    && { rm -f "$PIDFILE"; hsay "stopped (SIGTERM; pid $pid verified gone)"; return 0; }

  say "SIGTERM did not land; SIGKILL"
  kill -KILL "$pid" 2>/dev/null || true
  _wait_gone "$pid" 10 1 \
    && { rm -f "$PIDFILE"; hsay "stopped (SIGKILL; pid $pid verified gone)"; return 0; }

  die "pid $pid survived SIGKILL. $PIDFILE is left in place deliberately: removing it would
       hide a live VM from running(), and the next \`start\` would attach a second QEMU to
       $VM_DIR/disk.qcow2. Investigate (ps -fp $pid) and remove the pidfile only once the
       process is actually gone."
}

# True once $1 is observed gone; false if it is still alive after $2 polls of $3 seconds.
_wait_gone() {
  local pid="$1" tries="$2" gap="$3"
  for _ in $(seq 1 "$tries"); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep "$gap"
  done
  ! kill -0 "$pid" 2>/dev/null
}

# F-00 / ADR-010's four prerequisite probes, re-runnable rather than a one-off transcript.
# Exit status is the whole point: a non-zero exit means this guest does not satisfy the pin.
#
# The guest's own "ALL FOUR PROBES PASSED" banner is guest-authored text. The verdict this
# command prints is computed host-side from the exit status and carries the `==> [host]`
# sigil, a slot relayed guest bytes cannot occupy. That stops a *forged banner*; it does not
# make the guest's claims independently trustworthy, because ssh's exit status is itself the
# remote exit status -- see precondition 4 on F-08 in docs/tasks.md.
cmd_probes() {
  running || die "not running -- scripts/vm.sh start"
  local rc
  set +e
  # The single deliberate stdin-carrying ssh call in either script, opted into explicitly.
  MCP_VM_SSH_STDIN=1 cmd_ssh 'bash -s' 2>&1 <<'PROBES' | relay_guest
set -uo pipefail
fail=0
echo "### probe 1: kernel is GA 6.8 series, not HWE"
uname -r
uname -r | grep -Eq '^6\.8\.[0-9]+-[0-9]+-generic$' || { echo "FAIL: not a 6.8.x -generic kernel"; fail=1; }
dpkg -l | grep -E '^ii\s+linux-image' | awk '{print $2, $3}'
echo "-- HWE kernel packages present (expect none):"
if dpkg -l | awk '$1=="ii"{print $2}' | grep -E '^linux-(image|generic|headers).*hwe' ; then
  echo "FAIL: an HWE kernel package is installed"; fail=1
else
  echo "(none)"
fi
apt-cache policy linux-image-generic | sed -n '1,4p'

echo
echo "### probe 2: cgroups v2 controllers include memory, cpu, pids"
cat /sys/fs/cgroup/cgroup.controllers
for c in memory cpu pids; do
  grep -qw "$c" /sys/fs/cgroup/cgroup.controllers || { echo "FAIL: missing controller $c"; fail=1; }
done

echo
echo "### probe 3: six-namespace unshare"
if sudo unshare --mount --uts --ipc --net --pid --user --fork -- true; then
  echo "unshare exit=0"
else
  echo "FAIL: six-namespace unshare exited $?"; fail=1
fi

echo
echo "### probe 4: overlay mount with ADR-010's pinned options"
T=$(mktemp -d); mkdir -p "$T"/{lower,upper,work,merged,upper2,work2,merged2}
echo "lower-layer-content" > "$T/lower/pre-existing.txt"
sudo mount -t overlay overlay \
  -o "lowerdir=$T/lower,upperdir=$T/upper,workdir=$T/work,redirect_dir=off,metacopy=off,index=off" \
  "$T/merged" || { echo "FAIL: mount"; fail=1; }
echo "-- overlay filesystem registered (module loaded by the mount above):"
grep overlay /proc/filesystems || { echo "FAIL: overlay not in /proc/filesystems"; fail=1; }

echo "-- negative control: an unknown overlay option must be REJECTED"
# Without this, "the mount above was accepted with redirect_dir=off,metacopy=off,index=off"
# would be no evidence that those three were parsed at all -- a kernel that ignored unknown
# options would accept them just as happily. This kernel returns EINVAL, so acceptance IS
# parse evidence. Tested here rather than assumed.
if sudo mount -t overlay overlay \
     -o "lowerdir=$T/lower,upperdir=$T/upper2,workdir=$T/work2,mcp_bogus_option=42" \
     "$T/merged2" 2>/dev/null; then
  echo "FAIL: the kernel accepted an unknown overlay option, so option acceptance proves nothing"
  sudo umount "$T/merged2"; fail=1
else
  echo "   rejected, as required"
fi

echo "-- mount options as the kernel reports them:"
MNT="$(grep " $T/merged " /proc/mounts || true)"
printf '%s\n' "$MNT"
[ -n "$MNT" ] || { echo "FAIL: the pinned mount is absent from /proc/mounts"; fail=1; }
# Positive evidence the three pinned options are OFF, not merely that they were accepted: the
# kernel DOES echo redirect_dir=on / metacopy=on / index=on into /proc/mounts when they are
# set non-default, so their ABSENCE on this line is evidence they are off. That is a stronger
# oracle than the module-parameter block below, which is only a drift alarm on the
# kernel-wide defaults. `nouserxattr` is the expected branch for this privileged host-side
# mount -- ADR-010 pins `userxattr` conditionally, and this is the "do not set it" case.
for tok in redirect_dir=on metacopy=on index=on; do
  case "$MNT" in
    *"$tok"*) echo "FAIL: $tok is in effect on the pinned mount"; fail=1 ;;
  esac
done
case "$MNT" in
  *nouserxattr*) echo "   nouserxattr: present, as expected for a privileged mount" ;;
  *) echo "FAIL: expected nouserxattr on a privileged mount"; fail=1 ;;
esac

echo "-- overlay module defaults for the pinned options (asserted, not merely printed):"
for prm in redirect_dir metacopy index; do
  p="/sys/module/overlay/parameters/$prm"
  if [ -r "$p" ]; then
    v="$(cat "$p")"
    printf '   %-13s %s\n' "$prm" "$v"
    [ "$v" = "N" ] || { echo "FAIL: kernel-wide default for $prm is $v, expected N"; fail=1; }
  else
    echo "FAIL: $p is not readable"; fail=1
  fi
done
# xino_auto is NOT pinned by ADR-010. Printed for the record, deliberately not asserted.
if [ -r /sys/module/overlay/parameters/xino_auto ]; then
  printf '   %-13s %s   (not pinned; informational)\n' xino_auto \
    "$(cat /sys/module/overlay/parameters/xino_auto)"
fi

echo "-- lower-layer file read through the merge:"
cat "$T/merged/pre-existing.txt" || { echo "FAIL: lower read"; fail=1; }
echo "written-after-mount" | sudo tee "$T/merged/new-file.txt" >/dev/null
echo "-- post-mount write captured in upper:"
ls -l "$T/upper/"
cat "$T/upper/new-file.txt" || { echo "FAIL: upper capture"; fail=1; }
sudo umount "$T/merged" && echo "umount: clean"
grep -q " $T/merged " /proc/mounts && { echo "FAIL: still mounted"; fail=1; }
sudo rm -rf "$T"

echo
if [ "$fail" -eq 0 ]; then echo "ALL FOUR PROBES PASSED"; else echo "ONE OR MORE PROBES FAILED"; fi
exit $fail
PROBES
  rc=${PIPESTATUS[0]}
  set -e
  if [[ "$rc" -eq 0 ]]; then
    hsay "probes: PASS (guest exited 0; this line is the host's own, derived from that status)"
  else
    hsay "probes: FAIL (guest exited $rc) -- this guest does not satisfy the ADR-010 pin"
  fi
  return "$rc"
}

case "${1:-}" in
  start) cmd_start ;;
  wait) cmd_wait ;;
  ssh) shift; cmd_ssh "$@" ;;
  status) cmd_status ;;
  probes) cmd_probes ;;
  stop) cmd_stop ;;
  *) die "usage: $0 {start|wait|ssh [cmd...]|status|probes|stop}" ;;
esac
