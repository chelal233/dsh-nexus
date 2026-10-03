#!/usr/bin/env bash
set -euo pipefail

# Install the frozen original RPM. Optional business acceptance uses only
# isolated synthetic data; a denied sandbox or insufficient volume stops it.
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
  # Reserve 1GiB below the one-time authorized 24GiB peak for the polling interval and
  # transaction shutdown. Report the actual peak; sampling is not a hard quota.
  (( occupied <= 24696061952 && delivered <= 134217728 && free >= floor + 1073741824 ))
}

run_bounded() {
  local operation pid deadline reason code=0 stop_code=0 term_code=0 kill_code=0
  operation=$1; shift
  deadline=$((SECONDS + 125 * 60)); if test "$operation" = pull; then deadline=$((SECONDS + 20 * 60)); fi
  within_budget || { echo 'Storage envelope already exceeded'; return 1; }
  setsid "$@" &
  pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    reason=''
    if (( SECONDS >= deadline )); then
      reason='Shared execution deadline reached'
      printf '%s during %s\n' "$reason" "$operation" > "$evidence/execution-limit.txt"
    elif ! within_budget; then
      reason='Storage monitor failed or shutdown margin reached'
      printf '%s during %s\n' "$reason" "$operation" > "$evidence/storage-limit.txt"
    fi
    if test -n "$reason"; then
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
print(json.dumps({'scope': 'one-time approved24GiB temporary budget, original volume floor', 'total': d.total, 'free': d.free, 'floor': floor, 'estimate': 24 * 2**30}))
if d.free - 24 * 2**30 < floor:
    raise SystemExit('Insufficient native runner headroom; no download/install started')
PY
    mkdir -p "$temporary/incoming" "$evidence"
    # A release candidate is supplied as data, never interpolated shell code.
    # Freeze its exact bytes/source before the download, and bind it to this
    # checkout. Empty input retains the historical frozen QA comparison.
    python3 - "$evidence" <<'PY'
import json, os, pathlib, re, subprocess, sys
raw = os.environ.get('NEXUS_QA_CANDIDATE', '')
candidate = json.loads(raw) if raw else {
    'artifactId': 11149572256, 'runId': 36834335424,
    'commit': 'c34b51ccfe5467e76340e6e3bc1201d304cadde7',
    'zipBytes': 183377947, 'zipSha256': '5ce5253ea3aec8a96c6cc461bf02dfe880e49ccfa1c84bdb603599073a95a641',
    'rpmBytes': 184254885, 'rpmSha256': '049021576157dc97e253d9ba0c9e33e423feb090eb378d3550f40031a055d023',
}
assert set(candidate) == {'artifactId','runId','commit','zipBytes','zipSha256','rpmBytes','rpmSha256'}
for key in ('artifactId','runId','zipBytes','rpmBytes'):
    assert type(candidate[key]) is int and candidate[key] > 0
assert candidate['zipBytes'] <= 200 * 1024**2 and candidate['rpmBytes'] <= 200 * 1024**2
assert re.fullmatch('[a-f0-9]{40}', candidate['commit'])
for key in ('zipSha256','rpmSha256'):
    assert re.fullmatch('[a-f0-9]{64}', candidate[key])
if raw:
    assert candidate['commit'] == subprocess.check_output(['git','rev-parse','HEAD'], text=True).strip()
    repository = os.environ['GITHUB_REPOSITORY']
    assert repository == 'chelal233/dsh-nexus'
    def api(resource):
        return json.loads(subprocess.check_output(['gh','api',f'repos/{repository}/{resource}'], text=True))
    artifact = api(f"actions/artifacts/{candidate['artifactId']}")
    run = api(f"actions/runs/{candidate['runId']}")
    jobs = api(f"actions/runs/{candidate['runId']}/jobs?filter=latest&per_page=100")['jobs']
    assert artifact['name'] == 'qa-linux-x64-rpm' and not artifact['expired']
    assert artifact['size_in_bytes'] == candidate['zipBytes'] and artifact['digest'] == 'sha256:' + candidate['zipSha256']
    assert artifact['workflow_run']['id'] == candidate['runId'] and artifact['workflow_run']['head_sha'] == candidate['commit']
    assert run['head_sha'] == candidate['commit'] and run['name'] == 'Desktop build'
    assert run['status'] == 'completed' and run['conclusion'] == 'success'
    assert len(jobs) == 7 and len({job['name'] for job in jobs}) == 7
    assert all(job['status'] == 'completed' and job['conclusion'] == 'success' for job in jobs)
    (pathlib.Path(sys.argv[1]) / 'candidate-source.json').write_text(json.dumps({
        'artifact': artifact, 'run': {'id':run['id'],'head_sha':run['head_sha'],'conclusion':run['conclusion']},
        'jobs': [{'id':job['id'],'name':job['name'],'conclusion':job['conclusion']} for job in jobs],
    }, indent=2))
candidate['version'] = '1.0.4' if raw else '1.0.3'
(pathlib.Path(sys.argv[1]) / 'candidate-identity.json').write_text(json.dumps(candidate, indent=2))
PY
    df -B1 "$workspace" > "$evidence/initial-storage.txt"
    ;;
  verify)
    python3 - "$temporary" "$evidence" "${NEXUS_QA_BUSINESS:-0}" <<'PY'
import hashlib, json, pathlib, sys, zipfile
root, evidence = map(pathlib.Path, sys.argv[1:3])
candidate = json.loads((evidence / 'candidate-identity.json').read_text())
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
assert archive.stat().st_size == candidate['zipBytes']
assert digest == candidate['zipSha256']
version = candidate['version']
rpm = f'dsh-nexus_{version}_linux_x64.rpm'
build_name = f'dsh-nexus_{version}_linux_x64_build.json'
sums_name = f'dsh-nexus_{version}_linux_x64_SHA256SUMS.txt'
names = {rpm, build_name, sums_name, 'latest-x64-linux.yml'}
destination = root / 'verified'
destination.mkdir(exist_ok=False)
with zipfile.ZipFile(archive) as package:
    entries = package.infolist()
    assert len(entries) == 4 and {e.filename for e in entries} == names
    assert sum(e.file_size for e in entries) <= 200 * 1024**2
    assert all(not e.is_dir() and (e.external_attr >> 16) & 0o170000 != 0o120000 for e in entries)
    build = json.loads(package.read(build_name))
    assert build['commit'] == candidate['commit']
    assert build['target'] == 'x86_64-unknown-linux-gnu' and build['version'] == version
    assert build['buildId'] == f"electron-{candidate['runId']}-1-x86_64-unknown-linux-gnu"
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
            assert entry.file_size == candidate['rpmBytes']
            assert sha.hexdigest() == candidate['rpmSha256']
        verified.append({'name': entry.filename, 'bytes': entry.file_size, 'sha256': sha.hexdigest()})
(evidence / 'original-bytes.json').write_text(json.dumps({'result':'PASS','scope':'original ZIP and selected immutable RPM content/source, not native installation','artifactId':candidate['artifactId'],'zipSha256':digest,'build':build,'files':verified}, indent=2))
# This current-run duplicate is no longer needed; preserve the verified RPM.
archive.unlink()
if sys.argv[3] == '1':
    old_archives = list((root / 'incoming-old').glob('*.zip'))
    assert len(old_archives) == 1
    old_archive = old_archives[0]
    assert old_archive.stat().st_size == 183384334
    with old_archive.open('rb') as stream:
        old_digest = digest_stream(stream)
    assert old_digest == 'b4971deeef671d008705a91c8790dcee48219d96b8e9a3f69f83e4c24bb97b47'
    old_destination = root / 'verified-old'
    old_destination.mkdir(exist_ok=False)
    with zipfile.ZipFile(old_archive) as package:
        rpm = 'dsh-nexus_1.0.3_linux_x64.rpm'
        build_name = 'dsh-nexus_1.0.3_linux_x64_build.json'
        sums_name = 'dsh-nexus_1.0.3_linux_x64_SHA256SUMS.txt'
        names = {rpm, build_name, sums_name, 'latest-x64-linux.yml'}
        entries = package.infolist()
        assert len(entries) == 4 and {e.filename for e in entries} == names
        assert sum(e.file_size for e in entries) <= 200 * 1024**2
        assert all(not e.is_dir() and (e.external_attr >> 16) & 0o170000 != 0o120000 for e in entries)
        old_build = json.loads(package.read(build_name))
        assert old_build['commit'] == '81ae28ed65df7630f3b1aa3b5e0215383218341d'
        assert old_build['target'] == 'x86_64-unknown-linux-gnu' and old_build['version'] == '1.0.3'
        assert old_build['buildId'] == 'electron-36832892260-1-x86_64-unknown-linux-gnu'
        assert old_build['automatedChecks'] == 'passed' and old_build['installedPackageSmoke'] == 'passed-on-ci-runner'
        assert package.read(sums_name).decode() == ''.join(f"{f['sha256']}  {f['name']}\n" for f in old_build['files'])
        old_records = {f['name']: f['sha256'] for f in old_build['files']}
        old_verified = []
        for entry in entries:
            with package.open(entry) as source, (old_destination / entry.filename).open('xb') as output:
                sha = hashlib.sha256()
                for block in iter(lambda: source.read(1024 * 1024), b''):
                    output.write(block)
                    sha.update(block)
            if entry.filename in (rpm, 'latest-x64-linux.yml'):
                assert sha.hexdigest() == old_records[entry.filename]
            old_verified.append({'name': entry.filename, 'bytes': entry.file_size, 'sha256': sha.hexdigest()})
    (evidence / 'old-original-bytes.json').write_text(json.dumps({'result':'PASS','scope':'same-version different-build QA package; not an accepted public Release','artifactId':11148587625,'zipSha256':old_digest,'build':old_build,'files':old_verified}, indent=2))
    old_archive.unlink()
PY
    ;;
  environment)
    test -f "$evidence/original-bytes.json"
    command -v podman
    mkdir -p "$temporary/engine-home" "$temporary/engine-tmp" "$temporary/engine-run" \
      "$temporary/test/home" "$temporary/test/tmp" "$temporary/dnf-cache" "$temporary/engine-net" "$temporary/verified-old"
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
      -v "$temporary/verified-old:/qa/verified-old:ro" \
      -v "$temporary/test:/qa/test" -v "$temporary/dnf-cache:/qa/dnf-cache" -v "$evidence:/evidence" \
      -e GITHUB_ACTIONS=true -e "NEXUS_QA_BUSINESS=${NEXUS_QA_BUSINESS:-0}" \
      -e "NEXUS_QA_DEADLINE_MS=$(( $(date +%s) * 1000 + 120 * 60 * 1000 ))" "$image" bash -euo pipefail -c '
      rpms=(/qa/verified/dsh-nexus_*_linux_x64.rpm)
      test "${#rpms[@]}" = 1
      rpm_file=${rpms[0]}
      test -f "$rpm_file"
      package=$(rpm -qp --qf "%{NAME}" "$rpm_file")
      rpm -qp --qf "%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n" "$rpm_file" > /evidence/expected-nevra.txt
      if rpm -q "$package" > /evidence/before-install.txt 2>&1; then
        echo "Fresh container already contains the QA package"; exit 1
      fi
      dnf -y --setopt=cachedir=/qa/dnf-cache install \
        "$rpm_file" \
        xorg-x11-server-Xvfb xorg-x11-xauth xwd xdotool chromium procps-ng util-linux shadow-utils xdg-utils \
        gcc gcc-c++ glibc-devel make python3 cmake
      rpm -q --qf "%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n" "$package" > /evidence/installed-nevra.txt
      cmp /evidence/expected-nevra.txt /evidence/installed-nevra.txt
      rpm -qf "/opt/Nexus Launcher/nexus-launcher" > /evidence/installed-owner.txt
      rpm -V "$package" > /evidence/installed-verification.txt
      rpm -q --qf "%{NAME} %{VERSION}-%{RELEASE}.%{ARCH} source=%{SOURCERPM}\n" chromium > /evidence/browser-package.txt
      # Only the explicit package cache for this run, no host/user cache.
      dnf --setopt=cachedir=/qa/dnf-cache clean all
      useradd -u 1000 -d /qa/test/home -M nexusqa
      mkdir -p /evidence/gui
      chown -R 1000:1000 /qa/test /evidence/gui
      browser=$(command -v chromium-browser || command -v chromium)
      if test "$NEXUS_QA_BUSINESS" = 1; then
        # A normal default URI handler scoped to the synthetic QA HOME. The
        # official Open action still goes through Electron shell.openExternal.
        mkdir -p /qa/test/home/.local/share/applications /qa/test/home/.config
        printf "%s\n" "[Desktop Entry]" "Type=Application" "Name=Nexus QA Browser" \
          "Exec=$browser --user-data-dir=/qa/test/browser %U" "NoDisplay=true" \
          "MimeType=x-scheme-handler/http;x-scheme-handler/https;" \
          > /qa/test/home/.local/share/applications/nexus-qa-browser.desktop
        chown -R 1000:1000 /qa/test/home/.local /qa/test/home/.config
        for scheme in http https; do
          runuser -u nexusqa -- env HOME=/qa/test/home XDG_CONFIG_HOME=/qa/test/home/.config \
            XDG_DATA_HOME=/qa/test/home/.local/share xdg-mime default nexus-qa-browser.desktop "x-scheme-handler/$scheme"
          handler=$(runuser -u nexusqa -- env HOME=/qa/test/home XDG_CONFIG_HOME=/qa/test/home/.config \
            XDG_DATA_HOME=/qa/test/home/.local/share xdg-mime query default "x-scheme-handler/$scheme")
          test "$handler" = nexus-qa-browser.desktop
          printf "%s %s\n" "$scheme" "$handler" >> /evidence/default-browser.txt
        done
      fi
      run_qa() {
        runuser -u nexusqa -- env HOME=/qa/test/home TMPDIR=/qa/test/tmp \
          XDG_CONFIG_HOME=/qa/test/home/.config XDG_CACHE_HOME=/qa/test/home/.cache \
          XDG_DATA_HOME=/qa/test/home/.local/share NEXUS_QA_BROWSER="$browser" \
          NEXUS_QA_BUSINESS="$NEXUS_QA_BUSINESS" NEXUS_QA_PHASE="$1" \
          NEXUS_QA_DEADLINE_MS="$NEXUS_QA_DEADLINE_MS" xvfb-run -a -s "-screen 0 1280x800x24" \
          "/opt/Nexus Launcher/resources/runtime/node/bin/node" /source/linux-rpm-preflight.mjs
      }
      run_qa fresh
      if test "$NEXUS_QA_BUSINESS" = 1; then
        old_rpm=/qa/verified-old/dsh-nexus_1.0.3_linux_x64.rpm
        rpm -qp --qf "%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n" "$old_rpm" > /evidence/old-expected-nevra.txt
        # Normal local-RPM transactions, including dependencies and scriptlets.
        # Historical equal-NEVRA packages use reinstall;1.0.4 candidates exercise
        # a real downgrade to old1.0.3 and an upgrade back to the candidate.
        old_action=downgrade; final_action=upgrade
        if cmp -s /evidence/expected-nevra.txt /evidence/old-expected-nevra.txt; then
          old_action=reinstall; final_action=reinstall
        fi
        printf "%s %s\n" "$old_action" "$final_action" > /evidence/package-transaction-types.txt
        dnf -y --setopt=cachedir=/qa/dnf-cache "$old_action" "$old_rpm" > /evidence/replace-with-old.log 2>&1
        rpm -q --qf "%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n" "$package" > /evidence/old-installed-nevra.txt
        cmp /evidence/old-expected-nevra.txt /evidence/old-installed-nevra.txt
        rpm -V "$package" > /evidence/old-installed-verification.txt
        run_qa old
        dnf -y --setopt=cachedir=/qa/dnf-cache "$final_action" "$rpm_file" > /evidence/replace-with-final.log 2>&1
        rpm -q --qf "%{NAME} %{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n" "$package" > /evidence/final-installed-nevra.txt
        cmp /evidence/expected-nevra.txt /evidence/final-installed-nevra.txt
        rpm -V "$package" > /evidence/final-installed-verification.txt
        dnf --setopt=cachedir=/qa/dnf-cache clean all
        run_qa final
      fi
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
