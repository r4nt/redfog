# redfog

A [Moonlight](https://moonlight-stream.org/)/GameStream-compatible game streaming server for KDE Plasma on Wayland — see [design.md](design.md) for the full picture (architecture, goals, protocol details).

## Installing (Arch / CachyOS)

There's a local `PKGBUILD` under `packaging/arch/` — not yet published to the AUR, but buildable and installable directly from this checkout.

```bash
cd packaging/arch
makepkg -si
```

`-s` installs any missing `depends=`/`makedepends=` via pacman first, `-i` installs the resulting package. This also fetches and patches two dependencies not vendored into git (see `scripts/fetch-patched-deps.sh`), so the first build needs network access: `pam-sys` (a real dependency of `redfog-broker`'s PAM integration) and `moonlight-common-rust` (dev-only — used by `redfog-moonlight`'s own tests, never linked into anything shipped here — but still needed on disk to build *any* subset of the workspace, since it's registered as a workspace-wide `[patch]` in `Cargo.toml`, resolved before Cargo even looks at which specific packages are being built).

To also clean up the intermediate build directories afterward:

```bash
makepkg -sci
```

This installs four binaries (`redfog-server`, `redfog-broker`, `redfog-login`, `redfog-pair`), two systemd units, a dedicated `redfog` system user (created automatically via a `sysusers.d` entry), and `/etc/redfog/redfog.conf`.

### After installing

Arch packaging convention deliberately never auto-enables or auto-starts systemd services on install, so this is a manual step:

```bash
sudo systemctl enable --now redfog-server
```

`redfog-broker` (the privileged, cross-user session-spawning component) comes up automatically alongside it — `redfog-server.service` already declares it as a runtime dependency, so there's no need to separately enable it. Check both:

```bash
systemctl status redfog-server redfog-broker
journalctl -u redfog-server -u redfog-broker -f
```

Ports, the User-stage app (`REDFOG_USER_APP`, defaulting to your real KDE Plasma desktop), the compositor backend, and other options are all configurable via `/etc/redfog/redfog.conf` (a plain `EnvironmentFile`, restart both services after editing it).

Pairing/TLS identity and the paired-client list persist under `/var/lib/redfog-server/` across reboots and upgrades.
