# CommOS — quick start

This archive contains:

- `commosd`          — the CommOS single binary (control plane + media plane in one process)
- `pbx.example.yaml` — annotated runtime configuration to copy and edit
- `install.sh`       — one-shot LAN setup (auto-detects your IP, writes a `pbx.yaml`)
- `LICENSE`

## Fastest path (LAN test)

```sh
./install.sh --bin ./commosd --media-ip <THIS-HOST-LAN-IP>
# writes a pbx.yaml (SQLite, zero-dependency) and prints the command to run
```

## Manual

1. Copy and edit the config — at minimum set the phone-facing interface IP:

   ```sh
   cp pbx.example.yaml pbx.yaml
   # set  sip_listen: "<lan-ip>:5060"   and   media_ip: "<lan-ip>"
   ```

   `commosd` reads `./pbx.yaml` by default, or pass `--config <path>`. `media_ip` **must** be
   the interface the phones reach — the default (loopback) means calls connect with no audio.

2. Run it:

   ```sh
   ./commosd --config pbx.yaml
   ```

3. Point phones / tools at:
   - **SIP registrar:** `<lan-ip>:5060` (UDP)
   - **HTTP API + phone provisioning:** `http://<lan-ip>:8080`
     (set DHCP option 66 to `http://<lan-ip>:8080/provision`)

## Where do extensions / users / phones come from?

They are **not** in `pbx.yaml` — that file is infrastructure only (network binds, media address,
storage, auth). People, extensions, and devices live in the **provisioning directory** and are
created by:

- the **onboarding wizard** — auto-detects phones on the network, you approve, it binds each
  device to an extension; or
- the **REST API** — `POST /v1/{users,extensions,devices}`; or
- **config-as-code** — export the whole directory with `GET /v1/config` and re-import an edited
  copy with `POST /v1/config` (this is a *separate* directory YAML, not this runtime `pbx.yaml`).

See the repository README for the full API and onboarding walk-through.
