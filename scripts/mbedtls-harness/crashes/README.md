# Crash set

Empty after the structural sweep of 2026-10-24: **2 496 cases, 0 SIGSEGV, 0
abort, 0 canary clobbers** against the device `libmbedx509.so.0` (mbedtls 2.5.0).

Any future crash is saved here by `../sweep.py` as `NN-<tag>.der`, where the tag
carries the mutation and its byte offset (`len+7@123`, `form81@45`,
`flip#77@306`, …), and the tool immediately attaches `gdb-multiarch` to the
qemu gdbstub and prints `bt` plus the fault PC. Map that PC with:

```sh
mips-linux-gnu-nm -D /path/to/libmbedx509.so.0   # nearest exported symbol
```

The internal x509 helpers are stripped, so expect `mbedtls_x509_crt_parse_der+X`
rather than a source name.
