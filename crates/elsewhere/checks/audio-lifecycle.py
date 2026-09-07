"""Run in the Docker image as its desktop user, with a mounted release build.

python /src/crates/elsewhere/checks/audio-lifecycle.py /src/target/release/elsewhere
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1]).resolve())


def owned_audio(pid):
    found = {}
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            status = (proc / "status").read_text()
            if f"PPid:\t{pid}\n" not in status:
                continue
            env = dict(part.split(b"=", 1) for part in (proc / "environ").read_bytes().split(b"\0") if b"=" in part)
            root = env.get(b"PIPEWIRE_RUNTIME_DIR", b"").decode()
            if root.startswith("/tmp/elsewhere-audio-"):
                found[int(proc.name)] = Path(root)
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            pass
    return found


def run_case(name, *, replacement=None, no_audio=False, during_startup=False, group_signal=False, kill_service=None, missing_plugin=False, missing_home=False, freeze_worker=False):
    with tempfile.TemporaryDirectory(prefix="elsewhere-audio-check-") as directory:
        root = Path(directory)
        for folder in ("home", "runtime", "bin"):
            (root / folder).mkdir(mode=0o700)
        env = os.environ.copy()
        env.update(HOME=str(root / "home"), XDG_RUNTIME_DIR=str(root / "runtime"),
                   XDG_CONFIG_HOME=str(root / "config"))
        if missing_home:
            env.pop("HOME", None)
            env.pop("XDG_CONFIG_HOME", None)
        if missing_plugin:
            plugins = root / "plugins"
            plugins.mkdir()
            for plugin in [*Path("/usr/lib").glob("gstreamer-1.0/*.so"), *Path("/usr/lib").glob("*/gstreamer-1.0/*.so")]:
                if "pipewire" not in plugin.name:
                    (plugins / plugin.name).symlink_to(plugin)
            env.update(GST_PLUGIN_SYSTEM_PATH=str(plugins), GST_PLUGIN_PATH="", GST_REGISTRY=str(root / "registry.bin"))
        if replacement:
            program, body = replacement
            wrapper = root / "bin" / program
            wrapper.write_text("#!/bin/sh\n" + body + "\n")
            wrapper.chmod(0o755)
            env["PATH"] = str(root / "bin") + ":" + env["PATH"]
        args = [binary, "--no-tls", "--no-rtc", "--render-node", "none",
                "--listen", "127.0.0.1:8088", "--socket-name", "wayland-audio-check"]
        if no_audio:
            args.append("--no-audio")
        with (root / "log").open("w") as log:
            process = subprocess.Popen(args, env=env, stdout=log, stderr=log, start_new_session=True)
        owned = {}
        frozen = False
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                owned.update(owned_audio(process.pid))
                if freeze_worker and not frozen:
                    for pid in owned:
                        try:
                            if b"--audio-worker" in (Path("/proc") / str(pid) / "cmdline").read_bytes().split(b"\0"):
                                os.kill(pid, signal.SIGSTOP)
                                frozen = True
                                break
                        except FileNotFoundError:
                            pass
                text = (root / "log").read_text()
                if missing_home and process.poll() is not None:
                    assert process.returncode == 1, "configuration error did not exit cleanly"
                    assert "compositor ready" not in text, "compositor started before configuration validation"
                    assert not any(path.exists() for path in owned.values()), "audio directory survived configuration error"
                    print(f"{name}: passed", flush=True)
                    return
                if during_startup and owned:
                    break
                if "compositor ready" in text:
                    break
                assert process.poll() is None, f"{name}: desktop exited during startup"
                time.sleep(.001 if freeze_worker else .02)
            else:
                raise AssertionError(f"{name}: startup exceeded 20 seconds")
            if not during_startup:
                assert "compositor ready" in text, name
                if no_audio:
                    assert not owned, "no-audio started services"
                elif replacement or missing_plugin or freeze_worker:
                    assert "audio unavailable" in text, "failed audio was not reported"
                    if freeze_worker:
                        assert frozen and "native audio pipeline startup timed out" in text, "worker deadline was not exercised"
                else:
                    assert "audio unavailable" not in text, "audio failed to initialize"
                    assert len(owned) >= 4, "private services and worker not found"
                    private = next(iter(owned.values()))
                    for config_name in ("pipewire.conf", "pipewire-pulse.conf"):
                        assert (private / config_name).read_bytes() == (Path("/usr/share/pipewire") / config_name).read_bytes()
                    graph = json.loads(subprocess.check_output(
                        ["pw-dump"], env=dict(env, PIPEWIRE_REMOTE=str(private / "pipewire-0")), timeout=3))
                    modules = [obj["info"]["name"] for obj in graph if obj["type"].endswith(":Module")]
                    assert modules.count("libpipewire-module-profiler") == 1, modules
                    assert "libpipewire-module-jackdbus-detect" not in modules, modules
                    assert "libpipewire-module-x11-bell" not in modules, modules
                    nodes = [obj["info"]["props"].get("node.name") for obj in graph if obj["type"].endswith(":Node")]
                    for node_name in ("Dummy-Driver", "elsewhere-output", "elsewhere-microphone", "elsewhere-microphone-input"):
                        assert nodes.count(node_name) == 1, nodes
                    settings = {entry["key"]: entry["value"] for obj in graph for entry in obj.get("metadata", [])}
                    assert settings["clock.min-quantum"] == 1024, settings
                    assert settings["clock.force-quantum"] == 0, settings
            if kill_service:
                for pid in owned:
                    try:
                        command = (Path("/proc") / str(pid) / "cmdline").read_bytes().split(b"\0")[0]
                    except FileNotFoundError:
                        continue
                    if Path(os.fsdecode(command)).name == kill_service:
                        os.kill(pid, signal.SIGKILL)
                        break
                else:
                    raise AssertionError("service to kill not found")
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    if "audio unavailable" in (root / "log").read_text():
                        break
                    time.sleep(.02)
                else:
                    raise AssertionError("service failure was not reported")
                assert process.poll() is None, "audio failure stopped the desktop"
            if (replacement or missing_plugin or freeze_worker or kill_service) and not during_startup:
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline and any(path.exists() for path in owned.values()):
                    time.sleep(.02)
                assert process.poll() is None, "failed audio stopped desktop"
                assert not any(path.exists() for path in owned.values()), "failed audio resources survive while desktop runs"
                assert not any((Path("/proc") / str(pid)).exists() for pid in owned), "failed audio children survive while desktop runs"
            started = time.monotonic()
            if group_signal:
                os.killpg(process.pid, signal.SIGINT)
            else:
                process.send_signal(signal.SIGTERM)
            process.wait(timeout=3)
            if process.returncode != 0:
                Path("/tmp/elsewhere-audio-lifecycle-failure.log").write_text((root / "log").read_text())
            assert process.returncode == 0, f"{name}: exit {process.returncode}, see /tmp/elsewhere-audio-lifecycle-failure.log"
            for pid, private_root in owned.items():
                assert not (Path("/proc") / str(pid)).exists(), f"{name}: child survived"
                assert not private_root.exists(), f"{name}: private directory survived"
            print(f"{name}: passed, shutdown {time.monotonic() - started:.2f}s", flush=True)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            # Failure cleanup is limited to this test's recorded children.
            for pid in owned:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass


if __name__ == "__main__":
    run_case("idle startup and SIGTERM")
    run_case("foreground Ctrl+C", group_signal=True)
    run_case("no-audio", no_audio=True)
    run_case("missing service", replacement=("wireplumber", "exit 127"))
    run_case("partial startup failure", replacement=("pipewire-pulse", "exit 127"))
    run_case("missing native plugin", missing_plugin=True)
    run_case("missing home configuration", missing_home=True)
    run_case("readiness timeout", replacement=("pipewire", 'if [ "$1" = --version ]; then exec /usr/bin/pipewire "$@"; fi\nexec sleep 60'))
    run_case("SIGTERM during startup", replacement=("pipewire", 'if [ "$1" = --version ]; then exec /usr/bin/pipewire "$@"; fi\nexec sleep 60'), during_startup=True)
    run_case("worker readiness timeout", freeze_worker=True)
    for service in ("pipewire", "pipewire-pulse", "wireplumber"):
        run_case(f"{service} exit", kill_service=service)
