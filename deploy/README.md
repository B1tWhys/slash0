# Deploying slash0
- CI builds a `distroless/static-debian13` image on merges to master
- The server is running that in podman
- After the container build/push, CI ssh's into the host and bounces the service, pulling in the new `live` tag


### Server setup
Everything's under the `slash0` service user. Systemd owns the service lifecycle via a quadlet unit. All the service
configs/files live in `/home/slash0`, the one exception being `/var/log/slash0` (see Logging). Tearing the service down
is `userdel -r slash0` plus removing the linger file and the log dir.

## One-time host setup

Requires `podman` 5.0 or newer for quadlet's `Pull=` support.

1. Setup the user/service
```
useradd --system --create-home --home-dir /home/slash0 --shell /bin/bash slash0

# Enable lingering so the systemd user manager keeps running after the SSH session ends
loginctl enable-linger slash0

# Configure the service as needed
install -o slash0 -g slash0 -m 644 server.yaml.example /home/slash0/server.yaml

# Log dir, bind-mounted into the container by the quadlet unit. Nothing writes
# here yet, see Logging below.
install -d -o slash0 -g slash0 -m 750 /var/log/slash0


install -D -o slash0 -g slash0 -m 644 slash0.container /home/slash0/.config/containers/systemd/slash0.container

systemctl --user daemon-reload
systemctl --user enable --now slash0
```

2. Provision an SSH key and authorize it (make sure you edit `/home/slash0/.ssh/authorized_keys`, not your own auth'd
   keys). Ideally use `ssh-keygen -t ed25519`. Also it's a good idea to heavily restrict what cmds this user can run:

```
command="systemctl --user restart slash0",no-agent-forwarding,no-port-forwarding,no-pty,no-X11-forwarding ssh-ed25519 AAAA...
```

3. Configure the repo by adding the secret/value:

```bash
DEPLOY_SSH_KEY=<private key>
DEPLOY_HOST=slash0.dev
```

## Ops notes

```
systemctl --user status slash0
journalctl --user -u slash0 -f
```

To roll back, pin a known-good image in the quadlet unit. Every build is tagged
with its commit SHA alongside `:latest`:

```
Image=ghcr.io/b1twhys/slash0:<sha>
```

then `systemctl --user daemon-reload && systemctl --user restart slash0`.

### Logging

`/var/log/slash0` is bind-mounted read-write at the same path inside the container, so the server's log files land
directly in the host dir.

One constraint on that: don't switch the image to a `:nonroot` variant. The container runs as UID 0, which rootless
podman maps to the host `slash0` user, and that's the only reason files written into the mount come out owned by
`slash0`. A `:nonroot` image (UID 65532) maps into the subuid range instead and couldn't write to the dir at all.

The image is distroless and has no shell, so `podman exec ... sh` will not work.
When you need one for debugging, temporarily point the unit at the `:debug` tag
of the same base by rebuilding against `gcr.io/distroless/cc-debian13:debug`,
which adds busybox. Day to day, `journalctl` and the `/metrics` endpoint should
cover it.

If `systemctl --user` over SSH fails with "Failed to connect to bus", the
non-interactive session is missing `XDG_RUNTIME_DIR`. Check that `enable-linger` was run;
failing that, prefix the forced command with `XDG_RUNTIME_DIR=/run/user/$(id -u)`.
