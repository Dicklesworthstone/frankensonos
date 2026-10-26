import os, sys
from impacket.smbserver import SimpleSMBServer
PORT=int(sys.argv[1]); SHARE=os.path.expanduser("~/smbshare")
srv=SimpleSMBServer(listenAddress="0.0.0.0", listenPort=PORT)
srv.addShare("S", SHARE)
print("serving smb1", flush=True)
srv.start()
