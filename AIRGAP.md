# Airgapped install

Every release can be installed on a machine with no network access. engram also needs Qdrant and an
OpenAI-compatible inference endpoint reachable from that machine; neither is in the bundle.

## What to carry across

| File | Purpose |
|---|---|
| `engram-<version>-x86_64-unknown-linux-musl-offline.tar.gz` (or `aarch64-unknown-linux-gnu`) | binary, `install.sh`, systemd unit, nginx template, example configs, licences, `CHANGELOG.md`, `SHA256SUMS` |
| `install.sh` | the installer, also inside each bundle |
| `SHA256SUMS` | checksums of the release files |
| `engram-<version>-src-vendored.tar.gz` | optional: source with every dependency vendored |

```sh
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify engram-<version>-x86_64-unknown-linux-musl-offline.tar.gz --repo overcuriousity/engram
```

## Install

The installer never uses the network and never calls `sudo`.

```sh
sh install.sh --archive engram-<version>-x86_64-unknown-linux-musl-offline.tar.gz            # ~/.local/bin
sh install.sh --archive engram-<version>-x86_64-unknown-linux-musl-offline.tar.gz --system   # as root
```

`--system` installs `/usr/local/bin/engram`, the hardened unit and `/etc/engram/config.toml`. Every
file is checked against the bundle's `SHA256SUMS`; a changed file stops the install.

## TLS

engram listens on `127.0.0.1:8080`; nginx on the same machine serves https (`nginx.conf` in the
bundle). Use a certificate from your local CA, or a self-signed one:

```sh
openssl req -x509 -newkey rsa:3072 -sha256 -days 825 -nodes \
  -keyout /etc/ssl/private/engram.lan.key -out /etc/ssl/certs/engram.lan.pem \
  -subj "/CN=engram.lan" -addext "subjectAltName=DNS:engram.lan"
```

Then set the address users type:

```toml
[server]
public_url = "https://engram.lan"
```

Plain http to other machines is refused unless `allow_plain_http = true`; cookies then lose `Secure`
and the `__Host-` prefix, so prefer TLS.

Keep secrets out of `config.toml`; hand them to the service as credentials:

```sh
printf '%s' 'the-client-secret' | install -m 0600 /dev/stdin /etc/engram/oidc-secret
systemctl edit engram   # [Service] LoadCredential=oidc-secret:/etc/engram/oidc-secret
                        #           Environment=ENGRAM__AUTH__OIDC__CLIENT_SECRET_FILE=/run/credentials/engram.service/oidc-secret
```

## Build from source without network

```sh
tar -xzf engram-<version>-src-vendored.tar.gz
cd engram-<version>
cargo build --release --offline --locked
```

The default build runs under the unit's `MemoryDenyWriteExecute=yes`. The `contained` build (llama.cpp
in process) is for the Android app and is not run under this unit.
