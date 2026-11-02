#!/usr/bin/env python3
"""Fake gpu-screen-recorder for native recorder tests.

Serves GSR 6's IPC protocol on the -ipc socket. The control directory is the
socket's directory, so parallel tests stay isolated:
  fake-exit        exit at once with the code it contains
  fake-requests    every request received, one JSON object per line
  fake-no-replay   save-replay fails
  fake-no-regular  stop-replay-recording fails
  fake-hold        stop-replay-recording is not answered while it exists
  fake-stall       never log that capture started (a stuck portal)
"""
import json
import os
import select
import signal
import socket
import sys
import time

if sys.argv[1:2] == ["--version"]:
    print("6.1.3")
    sys.exit(0)
if sys.argv[1:2] == ["--list-audio-devices"]:
    print("default_output|Default output")
    print("default_input|Default input")
    print("alsa_output.pci.analog-stereo.monitor|Monitor of Built-in Analog Stereo")
    print("alsa_input.usb-mic|Fake USB Microphone")
    sys.exit(0)


def arg(flag):
    return sys.argv[sys.argv.index(flag) + 1]


path = arg("-ipc")
control = os.path.dirname(path)
replay_dir, regular_dir = arg("-o"), arg("-ro")


def exists(name):
    return os.path.exists(os.path.join(control, name))


if exists("fake-exit"):
    with open(os.path.join(control, "fake-exit")) as code:
        sys.exit(int(code.read()))


def quit_cleanly(*_):
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    sys.exit(0)


signal.signal(signal.SIGINT, quit_cleanly)
signal.signal(signal.SIGTERM, quit_cleanly)

server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
server.listen(8)
if not exists("fake-stall"):
    print("gsr info: gsr_capture_portal_start: pipewire negotiation finished",
          file=sys.stderr, flush=True)
clients = {}
held = []
saved = {"Replay": 0, "Video": 0}
# Like GSR's capture loop, a start applies a moment after its reply.
recording_from = None


def reply(client, request_id, ok, data=None):
    message = {"id": request_id, "result": "ok" if ok else "error"}
    if data is not None:
        message["data"] = data
    try:
        client.sendall((json.dumps(message) + "\n").encode())
    except OSError:
        pass


def save(directory, prefix):
    saved[prefix] += 1
    media = os.path.join(directory, f"{prefix}_{saved[prefix]}.mkv")
    with open(media, "wb") as file:
        file.write(b"fake gsr media")
    return media


def stop(client, request_id):
    global recording_from
    recording_from = None
    if exists("fake-no-regular"):
        reply(client, request_id, False, "failed to save the recording")
    else:
        reply(client, request_id, True, save(regular_dir, "Video"))


def handle(client, request):
    global recording_from
    with open(os.path.join(control, "fake-requests"), "a") as log:
        log.write(json.dumps(request) + "\n")
    name, request_id = request["name"], request["id"]
    if name == "save-replay":
        if exists("fake-no-replay"):
            reply(client, request_id, False, "failed to find a video keyframe")
        else:
            reply(client, request_id, True, save(replay_dir, "Replay"))
    elif name == "start-replay-recording":
        reply(client, request_id, True)
        if recording_from is None:
            recording_from = time.monotonic() + 0.05
    elif name == "stop-replay-recording":
        if recording_from is None or time.monotonic() < recording_from:
            reply(client, request_id, False, "no recording is running")
        elif exists("fake-hold"):
            held.append((client, request_id))
        else:
            stop(client, request_id)
    else:
        reply(client, request_id, False, "unknown request")


while True:
    readable, _, _ = select.select([server, *clients], [], [], 0.02)
    for sock in readable:
        if sock is server:
            client, _ = server.accept()
            clients[client] = b""
            continue
        data = sock.recv(4096)
        if not data:
            del clients[sock]
            sock.close()
            continue
        clients[sock] += data
        while b"\n" in clients[sock]:
            line, clients[sock] = clients[sock].split(b"\n", 1)
            handle(sock, json.loads(line))
    if held and not exists("fake-hold"):
        for client, request_id in held:
            stop(client, request_id)
        held = []
