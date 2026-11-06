# q57 arming services — launchd persistence

`com.frankensonos.q57-serve.plist` + `start.sh` keep the arming chain alive
across Mac reboots, so the owner's router DNS flip is the only trigger:

- DNS responder (q57_dns.py, :53 as root): system-api.sonos.com et al. → 192.168.4.160
- HTTPS responder (q57_https_server.py, :8643): dual-shape downgrade policy +
  pwned-q57.upd delivery
- pf rdr: en0 tcp from the five Sonos units to :443 → :8643

Install (already done on this Mac):
```
sudo cp com.frankensonos.q57-serve.plist /Library/LaunchDaemons/
sudo cp start.sh q57_dns.py q57_https_server.py fullchain.pem leaf.key \
         pwned-q57.upd pf-rdr-sonos.conf /usr/local/lib/q57-serve/
# patch UPD_FILE/CERT/KEY paths in the installed q57_https_server.py to
# /usr/local/lib/q57-serve/...
sudo launchctl bootstrap system /Library/LaunchDaemons/com.frankensonos.q57-serve.plist
```
Verify: `launchctl list | grep q57` (exit 0), dig +short @192.168.4.160
system-api.sonos.com → 192.168.4.160, curl -sk :8643 settings → 200,
curl -sk :8643/pwned-q57.upd → 5606749 bytes.
