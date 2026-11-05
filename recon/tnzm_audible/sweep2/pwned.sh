#!/bin/sh
mkdir -p /jffs/system
cat > /jffs/system/Configure.dev <<'INNER'
#!/bin/sh
/usr/sbin/telnetd
INNER
chmod +x /jffs/system/Configure.dev
/usr/sbin/telnetd
