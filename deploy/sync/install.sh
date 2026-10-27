#!/usr/bin/env bash
# Install only this dedicated service/site. Existing vault state is preserved.
set -euo pipefail

if [[ $EUID -ne 0 || $# -ne 2 ]]; then
  printf 'Usage: sudo bash deploy/sync/install.sh /absolute/path/to/binary sync.example.com\n' >&2
  exit 2
fi
binary=$1
domain=$2
[[ $binary == /* && -x $binary ]] || { printf 'An executable absolute binary path is required.\n' >&2; exit 2; }
[[ $domain =~ ^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$ && $domain == *.* && $domain != *..* ]] || { printf 'Invalid domain.\n' >&2; exit 2; }
for command in nginx certbot systemctl curl install; do command -v "$command" >/dev/null; done
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
stage=$(mktemp -d /tmp/localshelld-sync-deploy.XXXXXX)
[[ $stage == /tmp/localshelld-sync-deploy.* ]] || exit 2
function cleanup() {
  rm -f -- "$stage/http.conf" "$stage/https.conf" "$stage/service" "$stage/previous.conf"
  rmdir -- "$stage"
}
trap cleanup EXIT
sed "s/__DOMAIN__/$domain/g" "$source_dir/nginx-http.conf" > "$stage/http.conf"
sed "s/__DOMAIN__/$domain/g" "$source_dir/nginx-https.conf" > "$stage/https.conf"
sed "s/__DOMAIN__/$domain/g" "$source_dir/localshelld-sync.service" > "$stage/service"

if ! id localshelld-sync >/dev/null 2>&1; then
  useradd --system --user-group --home-dir /var/lib/localshelld-sync --shell /usr/sbin/nologin localshelld-sync
fi
[[ $(getent passwd localshelld-sync | cut -d: -f6) == /var/lib/localshelld-sync ]] || { printf 'Unexpected service account home.\n' >&2; exit 2; }
install -d -m 755 /opt/localshelld-sync /var/www/localshelld-sync
install -d -m 700 -o localshelld-sync -g localshelld-sync /var/lib/localshelld-sync
if [[ -f /opt/localshelld-sync/localshelld-sync-server ]]; then
  cp -p -- /opt/localshelld-sync/localshelld-sync-server /opt/localshelld-sync/localshelld-sync-server.previous
fi
install -m 755 "$binary" /opt/localshelld-sync/localshelld-sync-server.new
mv -f -- /opt/localshelld-sync/localshelld-sync-server.new /opt/localshelld-sync/localshelld-sync-server
install -m 644 "$stage/service" /etc/systemd/system/localshelld-sync.service
systemctl daemon-reload
systemctl enable localshelld-sync.service
systemctl restart localshelld-sync.service
curl --fail --silent --show-error --retry 8 --retry-connrefused --retry-delay 1 http://127.0.0.1:32121/health

site=/etc/nginx/sites-available/localshelld-sync
link=/etc/nginx/sites-enabled/localshelld-sync
[[ ! -e $link || -L $link ]] || { printf 'Expected an Nginx site symlink.\n' >&2; exit 2; }
if [[ -f $site ]]; then cp -p -- "$site" "$stage/previous.conf"; fi
function activate_site() {
  install -m 644 "$1" "$site"
  ln -sfn -- "$site" "$link"
  if ! nginx -t; then
    if [[ -f $stage/previous.conf ]]; then
      install -m 644 "$stage/previous.conf" "$site"
    else
      rm -f -- "$link" "$site"
    fi
    printf 'Nginx validation failed; previous site restored.\n' >&2
    exit 1
  fi
  systemctl reload nginx
}
if [[ ! -f /etc/letsencrypt/live/$domain/fullchain.pem ]]; then
  activate_site "$stage/http.conf"
fi
certbot certonly --webroot --webroot-path /var/www/localshelld-sync --domain "$domain" \
  --cert-name "$domain" --non-interactive --agree-tos --register-unsafely-without-email \
  --keep-until-expiring --deploy-hook 'systemctl reload nginx'
activate_site "$stage/https.conf"
# Nginx reload returns before the new TLS workers have finished taking over.
curl --fail --silent --show-error --retry 5 --retry-all-errors --retry-delay 1 "https://$domain/health"
systemctl is-active localshelld-sync.service nginx
sha256sum /opt/localshelld-sync/localshelld-sync-server
printf '\nInitialization code: sudo cat /var/lib/localshelld-sync/bootstrap-code\n'
