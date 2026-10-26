import os, sys, time
from impacket.smbserver import SimpleSMBServer, SMBCommands
PORT=int(sys.argv[1]); SHARE=os.path.expanduser("~/smbshare")
STALL=float(os.environ.get("STALL","600"))
ORIG=SMBCommands.smbComReadAndX

def patched(connId, smbServer, SMBCommand, recvPacket):
    print("stall", connId, flush=True)
    time.sleep(STALL)
    return ORIG(connId, smbServer, SMBCommand, recvPacket)

SMBCommands.smbComReadAndX = staticmethod(patched)
srv=SimpleSMBServer(listenAddress="0.0.0.0", listenPort=PORT)
srv.addShare("S", SHARE)
print("serving stall", flush=True)
srv.start()
