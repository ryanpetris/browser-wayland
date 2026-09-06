"""Docker check: two desktops, an unrelated audio graph, and saved mpv devices."""
import array
import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import wave

binary = "/src/target/release/browser-wayland"
children = []
owned_audio = {}


def remember_audio():
    parents = {child.pid for child in children}
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            status = (proc / "status").read_text()
            if not any(f"PPid:\t{pid}\n" in status for pid in parents):
                continue
            env = dict(part.split(b"=", 1) for part in (proc / "environ").read_bytes().split(b"\0") if b"=" in part)
            private = env.get(b"PIPEWIRE_RUNTIME_DIR", b"").decode()
            if private.startswith("/tmp/bw-audio-"):
                owned_audio[int(proc.name)] = private
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            pass


def start(args, env, log):
    with log.open("a") as output:
        child = subprocess.Popen(args, env=env, stdout=output, stderr=output)
    children.append(child)
    return child


def stop(child):
    if child.poll() is None:
        child.terminate()
        child.wait(timeout=3)


def wait_for(predicate):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.05)
    raise AssertionError("readiness timeout")


def output(args, env):
    return subprocess.check_output(args, env=env, timeout=3, stderr=subprocess.DEVNULL)


def graph(env):
    return json.loads(output(["pw-dump"], env))


def defaults(env):
    return [line for line in output(["pactl", "info"], env).decode().splitlines() if line.startswith(("Default Sink:", "Default Source:"))]


def amplitudes(env, sink):
    # A native monitor capture measures routing independently of the viewer decoder.
    with subprocess.Popen(["pw-record", "--target", sink, "--properties", "stream.capture.sink=true",
                           "--raw", "--format=f32", "--rate=48000", "--channels=1", "--latency=20ms", "-"],
                          env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as capture:
        try:
            raw, error = capture.communicate(timeout=.5)
        except subprocess.TimeoutExpired:
            capture.send_signal(signal.SIGINT)
            try:
                raw, error = capture.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                capture.kill()
                capture.communicate()
                raise
        # pw-record returns 1 on SIGINT on the validated PipeWire versions.
        assert capture.returncode in (0, 1), (capture.returncode, error, len(raw))
    samples = array.array("f", raw)
    assert len(samples) >= 9600, len(samples)
    samples = samples[-4800:]
    return [2 * abs(sum(sample * complex(math.cos(2 * math.pi * hz * i / 48000),
                                        math.sin(2 * math.pi * hz * i / 48000))
                        for i, sample in enumerate(samples))) / len(samples) for hz in (440, 880, 1320)]


with tempfile.TemporaryDirectory(prefix="bw-isolation-") as directory:
    root = Path(directory)
    try:
        outside = root / "outside"
        outside.mkdir(mode=0o700)
        outside_env = dict(os.environ, HOME=str(outside), XDG_RUNTIME_DIR=str(outside),
                           XDG_CONFIG_HOME=str(outside / "config"), XDG_STATE_HOME=str(outside / "state"),
                           PIPEWIRE_RUNTIME_DIR=str(outside), PIPEWIRE_REMOTE=str(outside / "pipewire-0"),
                           PIPEWIRE_CONFIG_DIR=str(outside), WIREPLUMBER_CONFIG_DIR=str(outside),
                           PULSE_RUNTIME_PATH=str(outside / "pulse"), PULSE_SERVER="unix:" + str(outside / "pulse/native"))
        for key in ("PIPEWIRE_NODE", "PULSE_SINK", "PULSE_SOURCE", "PIPEWIRE_CONFIG_NAME", "PIPEWIRE_CONFIG_PREFIX"):
            outside_env.pop(key, None)
        configs = Path("/src/crates/bw/src/audio")
        for name in ("pipewire.conf", "pipewire-pulse.conf"):
            (outside / name).write_text((configs / name).read_text().replace("browser-wayland-", "outside-"))
        shutil.copy("/usr/share/pipewire/client.conf", outside / "client.conf")
        (outside / "wireplumber.conf").write_text(Path("/usr/share/wireplumber/wireplumber.conf").read_text() + "\n" + (configs / "wireplumber.conf").read_text())
        for args in (["pipewire"], ["wireplumber", "--profile=browser-wayland"], ["pipewire-pulse"]):
            start(args, outside_env, outside / "services.log")
        def outside_ready():
            try:
                current = defaults(outside_env)
                return current == ["Default Sink: outside-output", "Default Source: outside-microphone"]
            except subprocess.SubprocessError:
                return False
        wait_for(outside_ready)
        external_defaults = defaults(outside_env)
        external_player = start(["gst-launch-1.0", "-q", "audiotestsrc", "is-live=true", "freq=1320", "volume=0.1", "!", "audioconvert", "!", "pulsesink"], outside_env, outside / "player.log")
        time.sleep(.4)
        assert amplitudes(outside_env, "outside-output")[2] > .05
        external_nodes = {o["id"] for o in graph(outside_env) if o["type"].endswith("Node")}
        sessions = []
        for index in range(2):
            home = root / str(index)
            home.mkdir()
            runtime = home / "runtime"
            runtime.mkdir(mode=0o700)
            env = dict(outside_env, HOME=str(home), XDG_CONFIG_HOME=str(home / "config"), XDG_RUNTIME_DIR=str(runtime))
            # The inherited selectors deliberately point at the unrelated graph.
            desktop = start([binary, "--no-tls", "--no-rtc", "--render-node", "none", "--listen", f"127.0.0.1:{8090 + index}",
                             "--socket-name", "wayland-isolation", "--exec", f"env -0 > {home}/client.env.tmp && mv {home}/client.env.tmp {home}/client.env"], env, home / "desktop.log")
            wait_for(lambda: (home / "client.env").exists())
            remember_audio()
            client = dict(part.split("=", 1) for part in (home / "client.env").read_text().split("\0") if "=" in part)
            assert client["PIPEWIRE_REMOTE"] != outside_env["PIPEWIRE_REMOTE"]
            assert defaults(client) == ["Default Sink: browser-wayland-output", "Default Source: browser-wayland-microphone"]
            assert all(not o.get("info", {}).get("props", {}).get("node.name", "").startswith("outside-") for o in graph(client))
            assert all(str(o.get("info", {}).get("props", {}).get("application.process.id")) != str(external_player.pid) for o in graph(client))
            wav = home / "tone.wav"
            with wave.open(str(wav), "wb") as audio:
                audio.setparams((1, 2, 48000, 48000, "NONE", "not compressed"))
                audio.writeframes(array.array("h", (int(6000 * math.sin(2 * math.pi * (440 + index * 440) * n / 48000)) for n in range(48000))).tobytes())
            config = home / "mpv"
            config.mkdir()
            protocol = "pipewire" if index == 0 else "pulse"
            (config / "mpv.conf").write_text(f"ao={protocol}\naudio-device={protocol}/browser-wayland-output\n")
            args = ["mpv", "--no-video", "--loop-file=inf", "--config-dir=" + str(config), str(wav)]
            player = start(args, client, home / "player.log")
            sessions.append((desktop, client, player, args, home))
        assert sessions[0][1]["PIPEWIRE_REMOTE"] != sessions[1][1]["PIPEWIRE_REMOTE"]
        time.sleep(1)
        for index, (_, env, player, _, _) in enumerate(sessions):
            assert player.poll() is None
            levels = amplitudes(env, "browser-wayland-output")
            assert levels[index] > .05 and all(level < .005 for n, level in enumerate(levels) if n != index), levels
            objects = graph(env)
            clients = {str(o["id"]): o["info"].get("props", {}) for o in objects if o["type"].endswith("Client")}
            streams = [o for o in objects if o.get("info", {}).get("props", {}).get("media.class") == "Stream/Output/Audio"
                       and str(o["info"]["props"].get("application.process.id", clients.get(str(o["info"]["props"].get("client.id")), {}).get("application.process.id"))) == str(player.pid)]
            assert len(streams) == 1, "application stream can be identified in the native graph"
            node = str(streams[0]["id"])
            output(["pw-cli", "set-param", node, "Props", "{ mute = true }"], env)
            assert max(amplitudes(env, "browser-wayland-output")) < .005
            output(["pw-cli", "set-param", node, "Props", "{ mute = false }"], env)
            assert amplitudes(env, "browser-wayland-output")[index] > .05
        assert defaults(outside_env) == external_defaults
        assert external_nodes <= {o["id"] for o in graph(outside_env) if o["type"].endswith("Node")}
        assert amplitudes(outside_env, "outside-output")[2] > .05
        print("two desktops have independent devices, defaults and tones; unrelated application is unaffected", flush=True)
        for index, (_, env, player, args, home) in enumerate(sessions):
            stop(player)
            time.sleep(.3)
            assert max(amplitudes(env, "browser-wayland-output")) < .005
            restarted = start(args, env, home / "player.log")
            time.sleep(.5)
            assert amplitudes(env, "browser-wayland-output")[index] > .05
            stop(restarted)
        print("native and Pulse mpv saved devices survive application exit/restart; exited applications leave silence", flush=True)
        stop(sessions[0][0])
        assert not Path(sessions[0][1]["PIPEWIRE_REMOTE"]).parent.exists()
        assert sessions[1][0].poll() is None
        assert defaults(sessions[1][1]) == ["Default Sink: browser-wayland-output", "Default Source: browser-wayland-microphone"]
        restarted = start(sessions[1][3], sessions[1][1], sessions[1][4] / "player.log")
        time.sleep(.5)
        assert amplitudes(sessions[1][1], "browser-wayland-output")[1] > .05
        assert defaults(outside_env) == external_defaults
        assert amplitudes(outside_env, "outside-output")[2] > .05
        print("stopping one desktop preserves the other desktop and unrelated playback", flush=True)
    except Exception:
        with Path("/tmp/bw-audio-isolation-failure.log").open("w") as report:
            for log in root.rglob("*.log"):
                report.write(log.name + "\n" + log.read_text() + "\n")
        raise
    finally:
        remember_audio()
        for child in reversed(children):
            try:
                stop(child)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        for pid, private in owned_audio.items():
            try:
                if ("PIPEWIRE_RUNTIME_DIR=" + private).encode() in (Path("/proc") / str(pid) / "environ").read_bytes().split(b"\0"):
                    os.kill(pid, signal.SIGKILL)
            except (FileNotFoundError, PermissionError, ProcessLookupError):
                pass
        for private in set(owned_audio.values()):
            shutil.rmtree(private, ignore_errors=True)
