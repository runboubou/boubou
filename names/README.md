# boubou-names

The service behind the names `boubou serve` prints for phones, such as
`192-168-1-20.<id>.lan.runboubou.com`. It answers DNS for `lan.runboubou.com`,
where each name points at the private address it spells, and gets Let's Encrypt
certificates for computers' names. Conversations never pass through it.

## Run it

It needs a Linux server with a public IPv4 address and ports 53 (UDP and TCP)
and 443 open.

1. In the `runboubou.com` zone, add `names` as an A record for the server's
   address (DNS only, not proxied) and `lan` as an NS record for
   `names.runboubou.com`.
2. Build and install it on the server:

   ```sh
   cargo build --locked --release -p boubou-names
   sudo install target/release/boubou-names /usr/local/bin/
   ```

3. Put the server's address in [boubou-names.service](boubou-names.service) and
   start it:

   ```sh
   sudo cp names/boubou-names.service /etc/systemd/system/
   sudo systemctl enable --now boubou-names
   ```

Try a new server against Let's Encrypt's staging environment first by adding
`--directory https://acme-staging-v02.api.letsencrypt.org/directory`. Each ACME
server keeps its own account and certificates in `/var/lib/boubou-names`.

`dig @names.runboubou.com 192-168-1-20.aaaaaaaaaaaaaaaa.lan.runboubou.com`
answers `192.168.1.20` once it runs, and `journalctl -u boubou-names` shows each
name it certifies.

## Limits

Let's Encrypt certifies 50 new names a week under `runboubou.com`; renewals do
not count. One network address may have 10 new names certified a day.
