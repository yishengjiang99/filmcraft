# Deploying the web build to grepawk.com/filmcraft

Live: <https://grepawk.com/filmcraft/>

```sh
scripts/deploy-grepawk.sh            # cargo xtask web, rsync target/web/dist -> root@64.23.239.208:/var/www/filmcraft
scripts/deploy-grepawk.sh --threads  # also build the threaded mt/ variant (nightly + rust-src)
```

- `index.html` uses relative URLs, so the plain `cargo xtask web` output works under `/filmcraft/`
  without a base-path setting.
- The script uploads into `/var/www/filmcraft.new`, then swaps it in (previous copy kept in
  `/var/www/filmcraft.prev`; roll back with `mv`).
- nginx is NOT configured here. The `/filmcraft/` location lives in the grepawk.com repo
  (`yishengjiang99/finalcut`, `nginx/finalcut-site.conf`, installed by
  `scripts/nginx/finalcut-site-nginx.sh` from its "Deploy grepawk.com" workflow). It sends, for
  `/filmcraft/` only: COOP `same-origin`, COEP `require-corp`, CORP `same-origin`,
  `application/wasm` for `.wasm`, gzip, and `Cache-Control: immutable` for `?v=<build>` URLs
  (`no-cache` otherwise). The page is therefore cross-origin isolated.
- Override target with `FILMCRAFT_DEPLOY_HOST` / `FILMCRAFT_DEPLOY_DIR`.
