# Running hvac on a NAS

`hvac` needs a working HEVC GPU encoder (`hevc_nvenc`, `hevc_vaapi`, or
`hevc_videotoolbox`) and an ffmpeg built with it. That combination is
unfortunately rare on NAS appliances by default — vendor ffmpeg builds
strip non-free encoders, the GPU is locked behind a kernel module the
vendor doesn't ship, or there's no GPU at all.

This doc lists what actually works on each common NAS, with copy-pasteable
commands.

If your NAS isn't listed here and you've got a working recipe, open a PR.

## Table of contents

- [Synology DSM](#synology) — Docker via Container Manager
- [QNAP QTS](#qnap) — Docker via Container Station
- [Unraid](#unraid) — Docker via Community Applications
- [OpenMediaVault](#openmediavault) — apt + compose plugin
- [TrueNAS SCALE](#truenas-scale) — apps catalog or direct Docker
- [TrueNAS CORE](#truenas-core) — off-box only
- [Off-box transcoding](#off-box) — NAS as storage, GPU host elsewhere

## When to use Docker on a NAS

Almost always.

NAS firmware does aggressive `rm -rf` on `/` during upgrades and reboots
(Unraid literally re-loads the rootfs from `/boot` at every boot), so a
binary you `curl … | sh`'d into `/usr/local/bin` is gone by next Tuesday.
The vendor's package manager either doesn't ship ffmpeg, ships a build
without `hevc_*` encoders, or refuses to coexist with the host. The
generic-Linux pattern of "install ffmpeg, install the driver, install the
binary" simply isn't reproducible on these appliances.

Docker side-steps all of it: the image bundles the right ffmpeg, the
VAAPI userland (Intel + AMD), and the hvac binary; the NAS only has
to expose `/dev/dri` and bind-mount your media path. NVIDIA is the
exception — the image deliberately does **not** bundle the NVIDIA
userspace driver (would bloat to ~2 GB and pin to one CUDA version),
so NVIDIA hosts inject it via `nvidia-container-toolkit` at runtime
with `--gpus all`. The Unraid / OMV sections below cover the toolkit
install.

The container image is published to GHCR by the
[`docker.yml`](../.github/workflows/docker.yml) workflow on every push
to `main` and on each tagged release. See the project
[`Dockerfile`](../Dockerfile) for what goes in it. If you'd rather
build from source: `git clone … && docker build -t hvac .` produces
an equivalent image.

## <a id="synology"></a>Synology DSM

**Hardware transcoding works on:** Plus-series (`+`), Value (`II+`,
`III+`), DVA, and a handful of older J-series with Intel CPUs that expose
`/dev/dri`. The ARM-based J-series (DS220j, DS223j, etc.) have no usable
HEVC encoder — run hvac off-box.

**Verify before you start:** SSH into DSM and run `ls -la /dev/dri`. If
you see `renderD128`, the iGPU is exposed and Docker can pass it
through. If the path is missing, your model doesn't have GPU
acceleration and you should jump to [off-box](#off-box).

### Container Manager (DSM 7.2+)

1. Open Package Center → install **Container Manager**.
2. Open Container Manager → **Registry** → search `ghcr.io/jackdanger/hvac`
   → download `latest`.
3. Open **Image** → select the downloaded image → **Run**.
4. In the wizard:
   - **General Settings:** name `hvac`, leave auto-restart off (this is a
     batch job, not a daemon).
   - **Advanced Settings → Volume:** add a bind mount from `/volume1/video`
     (or wherever your library lives) → `/media`.
   - **Advanced Settings → Device:** add `/dev/dri` (host) → `/dev/dri`
     (container).
   - **Advanced Settings → Execution Command:** override CMD to
     `--dry-run /media` for the first run.
5. Click **Done** and watch the log. A successful dry-run lists every
   file it would transcode; remove `--dry-run` on the next run.

### Equivalent docker-compose

If you prefer SSH + compose:

```yaml
# /volume1/docker/hvac/compose.yml
services:
  hvac:
    image: ghcr.io/jackdanger/hvac:latest
    container_name: hvac
    devices:
      - /dev/dri:/dev/dri          # Intel iGPU passthrough
    volumes:
      - /volume1/video:/media
    command: ["--dry-run", "/media"]
```

```bash
sudo docker compose -f /volume1/docker/hvac/compose.yml up
```

**Permissions note:** the image ships configured to run as UID 1026
GID 100 — DSM's default admin user and `users` group — so files
hvac writes back to `/volume1/video` are owned by your admin
account, not root. If your DSM admin is at a different UID (custom
setup, multiple admins), override with `user: "<uid>:<gid>"` in the
compose service. To find your UID, SSH in and run `id`.

## <a id="qnap"></a>QNAP QTS

**Hardware transcoding works on:** Intel-based TS-x53 / TS-x73 /
TS-h-series and similar. The HEVC encoder is `hevc_vaapi`; `/dev/dri`
is exposed by default on these models.

ARM-based QNAPs (TS-x28, TS-x32) have no usable HEVC encoder.

### Container Station

1. App Center → install **Container Station**.
2. Container Station → **Create** → **Create Application** →
   **YAML**:

```yaml
version: "3"
services:
  hvac:
    image: ghcr.io/jackdanger/hvac:latest
    container_name: hvac
    devices:
      - /dev/dri:/dev/dri
    volumes:
      - /share/Multimedia:/media
    command: ["--dry-run", "/media"]
```

3. **Create**. The container runs to completion; remove `--dry-run`
   for the real pass.

## <a id="unraid"></a>Unraid

Unraid's rootfs is a RAM-loaded squashfs that resets every boot, so
the only durable install path is Docker. Plugins handle the
GPU userland.

There's no Community Applications template for hvac yet (PRs to
[Squidly271/AppFeed](https://github.com/Squidly271/AppFeed) welcome),
so the path today is **Docker → Add Container** with the fields below,
or the equivalent `docker run` further down.

### Intel iGPU (most common)

1. Apps tab → install **Intel-GPU-TOP** (this exposes `/dev/dri`).
2. Docker tab → **Add Container**:
   - **Repository:** `ghcr.io/jackdanger/hvac:latest`
   - **Network Type:** Bridge
   - **Add another Path:** Container Path `/media`, Host Path `/mnt/user/media`
   - **Add another Device:** `/dev/dri`
   - **Post Arguments:** `--dry-run /media` for the first run.

### NVIDIA

1. Apps tab → install **Nvidia-Driver** (this needs a reboot).
2. Verify with `nvidia-smi` on the Unraid console.
3. Docker tab → **Add Container**, same fields as above except:
   - **Extra Parameters:** `--gpus all --runtime=nvidia`
   - **Variable:** `NVIDIA_VISIBLE_DEVICES=all`
   - (no `/dev/dri` device — that's the Intel path)

### docker run equivalent

```bash
# Intel
docker run --rm \
  --device /dev/dri:/dev/dri \
  -v /mnt/user/media:/media \
  ghcr.io/jackdanger/hvac:latest --dry-run /media

# NVIDIA
docker run --rm \
  --gpus all --runtime=nvidia \
  -v /mnt/user/media:/media \
  ghcr.io/jackdanger/hvac:latest --dry-run /media
```

## <a id="openmediavault"></a>OpenMediaVault

OMV is Debian under the hood, so the bare `install.sh` path **does**
work and you can run hvac directly on the host. That said, the
idiomatic OMV pattern is the **compose** plugin from `omv-extras`:

1. Install `omv-extras` (one-line install per the OMV docs).
2. Plugins → install **openmediavault-compose**.
3. Services → **Compose** → **Files** → add:

```yaml
services:
  hvac:
    image: ghcr.io/jackdanger/hvac:latest
    devices:
      - /dev/dri:/dev/dri          # if you have an Intel iGPU
    volumes:
      - /srv/dev-disk-by-uuid-xxx/media:/media
    command: ["--dry-run", "/media"]
```

NVIDIA needs the `nvidia-container-toolkit` apt package on the host
plus `runtime: nvidia` on the service.

## <a id="truenas-scale"></a>TrueNAS SCALE

SCALE is Debian + Kubernetes (k3s). Two options:

- **Apps catalog (preferred):** open Apps → **Discover Apps** →
  **Custom App** → paste the compose snippet from the
  [Synology](#synology) section, swapping the volume path for your
  TrueNAS dataset (typically `/mnt/poolname/media`).
- **Direct docker (post-25.04):** SCALE 25.04 ("Fangtooth") replaced
  k3s with plain Docker. The `docker run` commands from the
  [Unraid](#unraid) section work as-is — substitute the dataset path.

For NVIDIA GPUs: System Settings → **General** → **GPU**, then
**Isolated GPU PCI IDs** must NOT include the encoding GPU (the
opposite of the VM workflow).

## <a id="truenas-core"></a>TrueNAS CORE

CORE is FreeBSD. There are no pre-built hvac binaries for FreeBSD, and
the BSD ffmpeg doesn't ship `hevc_nvenc` / `hevc_vaapi`. Run hvac
[off-box](#off-box) and point it at the CORE share via NFS.

## <a id="off-box"></a>Off-box transcoding

If your NAS can't transcode itself — no GPU, ARM, FreeBSD, or just
a model the vendor crippled — run hvac on a separate Linux box or
Mac with a GPU and reach into the NAS over the network.

### NFS

On the GPU host:

```bash
sudo mkdir -p /mnt/nas-media
sudo mount -t nfs nas.local:/volume1/video /mnt/nas-media
hvac --dry-run /mnt/nas-media
```

Throughput on 1 GbE caps around 100 MB/s, comfortably faster than a
single GPU encode session — the bottleneck stays on the GPU, not the
wire.

### SMB / CIFS

```bash
sudo mount.cifs //nas.local/video /mnt/nas-media -o user=admin,vers=3.0
hvac --dry-run /mnt/nas-media
```

### `--probe-timeout`

Network mounts have a higher tail latency than local disks. If
ffprobe times out on a particularly cold file, raise the watchdog:

```bash
hvac --probe-timeout 120 /mnt/nas-media
```

The default is 30 s, which is plenty for warm shares but not for the
first read after a spindown on a cold NAS.

### Why files-in-place?

hvac overwrites originals by default. Over NFS/SMB that means the
final atomic rename happens on the NAS, not on your GPU host —
exactly where you want it. If you'd rather keep originals during a
trial run, add `--no-overwrite` and inspect the `.transcoded.*`
siblings before committing with `--replace`.

---

Found a NAS this doc doesn't cover? Open an issue with the model, the
output of `uname -a`, and `ls -la /dev/dri || true` — those three
pieces of info are enough to write a section for the next person.
