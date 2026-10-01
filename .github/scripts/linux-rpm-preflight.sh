#!/usr/bin/env bash
set -euo pipefail

# A prerequisite test, not full Harness acceptance. The original installer is
# never rebuilt; a denied sandbox or insufficient volume stops this test.
workspace=$(pwd -P)
temporary="$workspace/.codex-temp/linux-rpm-acceptance"
evidence="$workspace/.codex-artifacts/linux-rpm-acceptance"
image='registry.fedoraproject.org/fedora@sha256:07584325a68e0308215aff1de506d8da6452d2e465c25729829a01393446f204'
container='nexus-rpm-preflight'

guard_paths() {
  for directory in "$workspace" "$workspace/.codex-temp" "$temporary" "$workspace/.codex-artifacts" "$evidence"; do
    test ! -L "$directory" || { echo 'Redirected QA path'; exit 1; }
    if test -e "$directory"; then
      case "$(realpath "$directory")/" in "$workspace/"*) ;; *) echo 'QA path outside workspace'; exit 1 ;; esac
    fi
  done
}
guard_paths

# Use the runner's installed engine with private storage, without changing
# host services or making an engine socket/storage visible inside the test.
engine=(sudo env "HOME=$temporary/engine-home" "TMPDIR=$temporary/engine-tmp"
  "XDG_RUNTIME_DIR=$temporary/engine-run" podman
  --root "$temporary/engine-storage" --runroot "$temporary/engine-run"
  --tmpdir "$temporary/engine-tmp" --network-config-dir "$temporary/engine-net" --events-backend file)

measure_root() {
  local root before after measured
  root=$1
  test -d "$root" && test ! -L "$root" || return 2
  before=$(sudo stat -c '%d:%i' -- "$root") || return 2
  # Package managers replace temporary directory entries during transactions.
  # GNU find ignores only entries disappearing between readdir and stat; other
  # traversal failures still abort the operation. Count physical blocks once
  # per inode, without following symlinks or traversing engine overlay mounts.
  measured=$(sudo find "$root" -xdev -ignore_readdir_race -printf '%D %i %b\n' |
    awk -v root_inode="$before" '!seen[$1 ":" $2]++ { blocks += $3 } END { if (!seen[root_inode]) exit 1; printf "%.0f\n", blocks * 512 }') || return 2
  test -d "$root" && test ! -L "$root" || return 2
  after=$(sudo stat -c '%d:%i' -- "$root") || return 2
  test "$before" = "$after" && [[ "$measured" =~ ^[0-9]+$ ]] || return 2
  printf '%s\n' "$measured"
}

within_budget() {
  local occupied delivered total free floor volume
  occupied=$(measure_root "$temporary") || return 2
  delivered=$(measure_root "$evidence") || return 2
  volume=$(df -B1 --output=size,avail "$workspace" | tail -1) || return 2
  read -r total free <<< "$volume" || return 2
  [[ "$occupied $delivered $total $free" =~ ^[0-9]+\ [0-9]+\ [0-9]+\ [0-9]+$ ]] || return 2
  floor=$((total / 10)); if (( floor < 21474836480 )); then floor=21474836480; fi
  printf '%s %s %s\n' "$(date -u +%FT%TZ)" "$occupied" "$free" >> "$evidence/storage-timeline.txt"
  # Reserve 1GiB below the authorized 8GiB peak for the polling interval and
  # transaction shutdown. Report the actual peak; sampling is not a hard quota.
  (( occupied <= 7516192768 && delivered <= 134217728 && free >= floor + 1073741824 ))
}

run_bounded() {
  local operation pid code=0 stop_code=0 term_code=0 kill_code=0
  operation=$1; shift
  within_budget || { echo 'Storage envelope already exceeded'; return 1; }
  setsid "$@" &
  pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    if ! within_budget; then
      printf 'Storage monitor failed or shutdown margin reached during %s\n' "$operation" > "$evidence/storage-limit.txt"
      # Only this foreground operation's session and the uniquely named QA
      # container are owned here. Abort immediately, then verify its outcome.
      sudo kill -TERM -- "-$pid" || term_code=$?
      if "${engine[@]}" container exists "$container"; then
        "${engine[@]}" stop --time 10 "$container" >> "$evidence/budget-stop.txt" 2>&1 || stop_code=$?
      fi
      for attempt in $(seq 1 10); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
      done
      if kill -0 "$pid" 2>/dev/null; then sudo kill -KILL -- "-$pid" || kill_code=$?; fi
      wait "$pid" || code=$?
      printf 'operation=%s termExit=%s killExit=%s childExit=%s containerStopExit=%s\n' "$operation" "$term_code" "$kill_code" "$code" "$stop_code" >> "$evidence/budget-stop.txt"
      return 1
    fi
    sleep 2
  done
  wait "$pid" || code=$?
  within_budget || { echo 'Storage envelope exceeded at operation completion' > "$evidence/storage-limit.txt"; return 1; }
  return "$code"
}

case "${1:?phase required}" in
  capacity)
    python3 - "$workspace" <<'PY'
import json, shutil, sys
d = shutil.disk_usage(sys.argv[1])
floor = max(20 * 2**30, d.total // 10)
print(json.dumps({'scope': 'approved8GiB temporary budget, original volume floor', 'total': d.total, 'free': d.free, 'floor': floor, 'estimate': 8 * 2**30}))
if d.free - 8 * 2**30 < floor:
    raise SystemExit('Insufficient native runner headroom; no download/install started')
PY
    mkdir -p "$temporary/incoming" "$evidence"
    df -B1 "$workspace" > "$evidence/initial-storage.txt"
    ;;
  verify)
    python3 - "$temporary" "$evidence" <<'PY'
import hashlib, json, pathlib, sys, zipfile
root, evidence = map(pathlib.Path, sys.argv[1:])
archives = list((root / 'incoming').glob('*.zip'))
if len(archives) != 1:
    raise SystemExit('Expected one original artifact ZIP')
archive = archives[0]
def digest_stream(stream):
    value = hashlib.sha256()
    for block in iter(lambda: stream.read(1024 * 1024), b''):
        value.update(block)
    return value.hexdigest()
with archive.open('rb') as stream:
    digest = digest_stream(stream)
assert archive.stat().st_size == 183377947
assert digest == '5ce5253ea3aec8a96c6cc461bf02dfe880e49ccfa1c84bdb603599073a95a641'
rpm = 'dsh-nexus_1.0.3_linux_x64.rpm'
build_name = 'dsh-nexus_1.0.3_linux_x64_build.json'
sums_name = 'dsh-nexus_1.0.3_linux_x64_SHA256SUMS.txt'
names = {rpm, build_name, sums_name, 'latest-x64-linux.yml'}
destination = root / 'verified'
destination.mkdir(exist_ok=False)
with zipfile.ZipFile(archive) as package:
    entries = package.infolist()
    assert len(entries) == 4 and {e.filename for e in entries} == names
    assert sum(e.file_size for e in entries) <= 200 * 1024**2
    assert all(not e.is_dir() and (e.external_attr >> 16) & 0o170000 != 0o120000 for e in entries)
    build = json.loads(package.read(build_name))
    assert build['commit'] == 'c34b51ccfe5467e76340e6e3bc1201d304cadde7'
    assert build['target'] == 'x86_64-unknown-linux-gnu' and build['version'] == '1.0.3'
    assert build['buildId'] == 'electron-36834335424-1-x86_64-unknown-linux-gnu'
    assert build['automatedChecks'] == 'passed' and build['installedPackageSmoke'] == 'passed-on-ci-runner'
    assert package.read(sums_name).decode() == ''.join(f"{f['sha256']}  {f['name']}\n" for f in build['files'])
    records = {f['name']: f['sha256'] for f in build['files']}
    verified = []
    for entry in entries:
        with package.open(entry) as source, (destination / entry.filename).open('xb') as output:
            sha = hashlib.sha256()
            for block in iter(lambda: source.read(1024 * 1024), b''):
                output.write(block)
                sha.update(block)
        if entry.filename in (rpm, 'latest-x64-linux.yml'):
            assert sha.hexdigest() == records[entry.filename]
        if entry.filename == rpm:
            assert entry.file_size == 184254885
            assert sha.hexdigest() == '049021576157dc97e253d9ba0c9e33e423feb090eb378d3550f40031a055d023'
        verified.append({'name': entry.filename, 'bytes': entry.file_size, 'sha256': sha.hexdigest()})
(evidence / 'original-bytes.json').write_text(json.dumps({'result':'PASS','scope':'original ZIP and selected immutable RPM content/source, not native installation','artifactId':11149572256,'zipSha256':digest,'build':build,'files':verified}, indent=2))
PY
    ;;
  environment)
    test -f "$evidence/original-bytes.json"
    command -v podman
    mkdir -p "$temporary/engine-home" "$temporary/engine-tmp" "$temporary/engine-run" \
      "$temporary/test/home" "$temporary/test/tmp" "$temporary/dnf-cache" "$temporary/engine-net"
    printf '{"auths":{}}\n' > "$temporary/engine-home/auth.json"
    chmod 600 "$temporary/engine-home/auth.json"
    uname -a > "$evidence/runner-kernel.txt"
    cat /etc/os-release > "$evidence/runner-os.txt"
    "${engine[@]}" version > "$evidence/podman-version.txt"
    # Podman 3.x/4.9 reject workspace-contained runroot paths over 50 chars.
    # Use the supported runner's preinstalled version containing upstream's
    # path fix, not aliases outside the workspace or security configuration.
    version=$(podman version --format '{{.Client.Version}}')
    python3 - "$version" <<'PY'
import sys
parts = sys.argv[1].split('.')
if tuple(map(int, parts[:2])) < (5, 7):
    raise SystemExit('Preinstalled Podman5.7+ required for workspace-contained runroot')
PY
    "${engine[@]}" info > "$evidence/podman-info.txt"
    run_bounded pull "${engine[@]}" pull --authfile "$temporary/engine-home/auth.json" "$image"
    "${engine[@]}" image inspect "$image" > "$evidence/fedora-image.json"
    run_bounded install-and-gui "${engine[@]}" run --name "$container" --network bridge \
      -v "$workspace/.github/scripts:/source:ro" -v "$temporary/verified:/qa/verified:ro" \
      -v "$temporary/test:/qa/test" -v "$temporary/dnf-cache:/qa/dnf-cache" -v "$evidence:/evidence" \
      -e GITHUB_ACTIONS=true "$image" bash -euo pipefail -c '
      rpm_file=/qa/verified/dsh-nexus_1.0.3_linux_x64.rpm
      package=$(rpm -qp --qf "%{NAME}" "$rpm_file")
      rpm -qp --qf "%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n" "$rpm_file" > /evidence/expected-nevra.txt
      if rpm -q "$package" > /evidence/before-install.txt 2>&1; then
        echo "Fresh container already contains the QA package"; exit 1
      fi
      dnf -y --setopt=cachedir=/qa/dnf-cache install \
        "$rpm_file" \
        xorg-x11-server-Xvfb xorg-x11-xauth chromium procps-ng util-linux shadow-utils
      rpm -q --qf "%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n" "$package" > /evidence/installed-nevra.txt
      cmp /evidence/expected-nevra.txt /evidence/installed-nevra.txt
      rpm -qf "/opt/Nexus Launcher/nexus-launcher" > /evidence/installed-owner.txt
      rpm -V "$package" > /evidence/installed-verification.txt
      useradd -u 1000 -d /qa/test/home -M nexusqa
      mkdir -p /evidence/gui
      chown -R 1000:1000 /qa/test /evidence/gui
      browser=$(command -v chromium-browser || command -v chromium)
      runuser -u nexusqa -- env HOME=/qa/test/home TMPDIR=/qa/test/tmp \
        XDG_CONFIG_HOME=/qa/test/home/.config XDG_CACHE_HOME=/qa/test/home/.cache \
        XDG_DATA_HOME=/qa/test/home/.local/share NEXUS_QA_BROWSER="$browser" xvfb-run -a \
        "/opt/Nexus Launcher/resources/runtime/node/bin/node" \
        /source/linux-rpm-preflight.mjs
      ' 2>&1 | tee "$evidence/environment.log"
    test ! -e "$evidence/storage-limit.txt"
    ;;
  finish)
    if test -d "$evidence"; then
      cleanup_failed=0
      if test -d "$temporary/engine-storage"; then
        if "${engine[@]}" container exists "$container"; then
          inspect_before=0; "${engine[@]}" inspect "$container" > "$evidence/container-before-cleanup.json" 2> "$evidence/inspect-before-error.txt" || inspect_before=$?
          stop_code=0; "${engine[@]}" stop --time 10 "$container" > "$evidence/container-stop.txt" 2>&1 || stop_code=$?
          inspect_after=0; "${engine[@]}" inspect "$container" > "$evidence/container-after-stop.json" 2> "$evidence/inspect-after-error.txt" || inspect_after=$?
          running_code=0; running=$("${engine[@]}" inspect --format '{{.State.Running}}' "$container" 2> "$evidence/running-query-error.txt") || running_code=$?
          printf 'inspectBeforeExit=%s stopExit=%s inspectAfterExit=%s runningQueryExit=%s running=%s\n' "$inspect_before" "$stop_code" "$inspect_after" "$running_code" "$running" > "$evidence/cleanup-status.txt"
          if test "$inspect_before" -ne 0 || test "$inspect_after" -ne 0 || test "$running_code" -ne 0 || test "$stop_code" -ne 0 || test "$running" != false; then
            cleanup_failed=1
          fi
          # Normal rm refuses a still-running container. Attempt it even after
          # a failed inspection, without force and without hiding that error.
          remove_code=0; "${engine[@]}" rm "$container" > "$evidence/container-remove.txt" 2>&1 || remove_code=$?
          exists_code=0; "${engine[@]}" container exists "$container" || exists_code=$?
          printf 'removeExit=%s existsExit=%s\n' "$remove_code" "$exists_code" >> "$evidence/cleanup-status.txt"
          if test "$remove_code" -ne 0 || test "$exists_code" -ne 1; then cleanup_failed=1; fi
        else
          exists_code=$?
          printf 'containerExistsExit=%s\n' "$exists_code" > "$evidence/cleanup-status.txt"
          if test "$exists_code" -ne 1; then cleanup_failed=1; fi
        fi
      else
        echo 'Container engine never started' > "$evidence/cleanup-status.txt"
      fi
      sudo du -sx -B1 "$temporary" > "$evidence/final-temporary-size.txt"
      df -B1 "$workspace" > "$evidence/final-storage.txt"
      # Retain immutable archives only until this ephemeral runner is disposed;
      # upload evidence, never the RPM, runtime, synthetic data or engine cache.
      sudo chown -R "$(id -u):$(id -g)" "$evidence"
      test "$(du -sx -B1 "$evidence" | cut -f1)" -le 134217728
      test "$cleanup_failed" -eq 0
    fi
    ;;
  *) echo 'Unknown preflight phase'; exit 1 ;;
esac
