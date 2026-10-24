/* xmparse - offline harness for the device's own mbedtls x509 parser.
 *
 * Reads one DER certificate (and optionally a DER trust anchor), runs the real
 * mbedtls_x509_crt_parse_der from the device library, then optionally walks the
 * chain with mbedtls_x509_crt_verify so the signature paths (including
 * RSASSA-PSS, CVE-2018-0487) are reached. Prints the linked library version and
 * a canary around the certificate object so a heap overwrite is visible.
 *
 * build (cross):  clang --target=mips-linux-gnu -mabi=32 -march=mips32 \
 *                   -O1 -I<mbedtls-2.5.0/include> -c xmparse.c -o xmparse.o
 * link:           mips-linux-gnu-ld -m elf32btsmip -o xmparse xmparse.o \
 *                   -L<rootfs>/lib -lmbedtls -lmbedx509 -lmbedcrypto -lc \
 *                   --dynamic-linker=/lib/ld.so.1
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "mbedtls/version.h"
#include "mbedtls/x509_crt.h"

#define CANARY 0x5a5aa5a5u
#define PAD_PRE 64
#define PAD_POST 512

struct boxed {
    unsigned char pre[PAD_PRE];
    mbedtls_x509_crt crt;
    unsigned char post[PAD_POST];
};

static int slurp(const char *path, unsigned char **out, size_t *len)
{
    FILE *f = fopen(path, "rb");
    long n;
    if (!f) return -1;
    if (fseek(f, 0, SEEK_END) != 0) { fclose(f); return -1; }
    n = ftell(f);
    if (n <= 0) { fclose(f); return -1; }
    rewind(f);
    *out = (unsigned char *) malloc((size_t) n);
    if (!*out) { fclose(f); return -1; }
    if (fread(*out, 1, (size_t) n, f) != (size_t) n) { fclose(f); free(*out); return -1; }
    fclose(f);
    *len = (size_t) n;
    return 0;
}

static int canary_ok(const struct boxed *b)
{
    int i;
    for (i = 0; i < PAD_PRE; i++) if (b->pre[i] != (unsigned char) (CANARY & 0xff)) return 0;
    for (i = 0; i < PAD_POST; i++) if (b->post[i] != (unsigned char) ((CANARY >> 8) & 0xff)) return 0;
    return 1;
}

static void poison(struct boxed *b)
{
    memset(b->pre, CANARY & 0xff, PAD_PRE);
    memset(b->post, (CANARY >> 8) & 0xff, PAD_POST);
}

int main(int argc, char **argv)
{
    unsigned char *der = NULL, *ca_der = NULL;
    size_t len = 0, ca_len = 0;
    struct boxed *leaf, *ca = NULL;
    char info[1024];
    int ret, loop = 1, i, verify = 0;

    if (argc < 2) {
        fprintf(stderr, "usage: %s <leaf.der> [ca.der] [--loop N] [--info]\n", argv[0]);
        return 2;
    }

    for (i = 2; i < argc; i++)
        if (!strcmp(argv[i], "--loop") && i + 1 < argc) loop = atoi(argv[++i]);
    /* second positional = trust anchor */
    if (argc > 2 && argv[2][0] != '-') verify = 1;

    printf("headers: mbedtls %s\n", MBEDTLS_VERSION_STRING);
    printf("file: %s\n", argv[1]);

    if (slurp(argv[1], &der, &len) != 0) { fprintf(stderr, "cannot read %s\n", argv[1]); return 2; }
    printf("len: %u\n", (unsigned) len);

    leaf = (struct boxed *) calloc(1, sizeof *leaf);
    if (!leaf) return 2;
    if (verify && slurp(argv[2], &ca_der, &ca_len) != 0) {
        fprintf(stderr, "cannot read %s\n", argv[2]);
        return 2;
    }
    if (verify) {
        ca = (struct boxed *) calloc(1, sizeof *ca);
        if (!ca) return 2;
        poison(ca);
        mbedtls_x509_crt_init(&ca->crt);
        ret = mbedtls_x509_crt_parse_der(&ca->crt, ca_der, ca_len);
        printf("ca_parse_ret: %d (-0x%04x)\n", ret, (unsigned) -ret);
        printf("ca_canary: %s\n", canary_ok(ca) ? "ok" : "CLOBBERED");
    }

    for (i = 0; i < loop; i++) {
        poison(leaf);
        mbedtls_x509_crt_init(&leaf->crt);
        ret = mbedtls_x509_crt_parse_der(&leaf->crt, der, len);
        printf("parse_ret: %d (-0x%04x) canary: %s", ret, (unsigned) -ret,
               canary_ok(leaf) ? "ok" : "CLOBBERED");

        if (ret == 0) {
            unsigned int flags = 0;
            if (mbedtls_x509_crt_info(info, sizeof info - 1, "", &leaf->crt) > 0)
                printf(" | subject: %.120s", strstr(info, "subject") ? strstr(info, "subject") : "?");
            if (verify) {
                int v = mbedtls_x509_crt_verify(&leaf->crt, &ca->crt, NULL, NULL, &flags,
                                                NULL, NULL);
                printf(" | verify: %d flags:0x%x", v, flags);
            }
        }
        printf("\n");
        fflush(stdout);
        mbedtls_x509_crt_free(&leaf->crt);
    }

    free(leaf);
    if (ca) { mbedtls_x509_crt_free(&ca->crt); free(ca); }
    free(der);
    free(ca_der);
    return 0;
}
