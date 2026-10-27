/*
 * Offline x509 harness for the 57.23 device libmbedtls (bead frankensonos-wbz).
 *
 * Runs the DEVICE's own libmbedtls.so.14 / libmbedx509.so.1 / libmbedcrypto.so.7 under
 * qemu-ppc user mode, against the cross toolchain's glibc (the device .so only needs
 * GLIBC_2.0 / GLIBC_2.1.3, both still exported).
 *
 * The API sequence mirrors what anacapad actually calls (dlsym'd strings in the binary):
 *   mbedtls_x509_crt_init -> mbedtls_x509_crt_parse_der_nocopy
 *   -> mbedtls_x509_crt_verify_with_profile -> mbedtls_x509_crt_free
 * plus the DN/info printers, which are the over-read surfaces.
 *
 * usage: harness <mode> <der> [<ca.der>]
 *   modes: parse | nocopy | verify | chain | info | dn
 * exit: 0 ok, 1 parse error, 2 verify error, 3 usage, 4 alloc
 * A signal (139/135/136) is the interesting outcome.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "mbedtls/x509_crt.h"
#include "mbedtls/x509.h"
#include "mbedtls/error.h"
#include "mbedtls/platform.h"

/* Sonos addition (not in upstream 2.28): installs a time_t(*)(void) override that the
 * validity checks call instead of time(). anacapad installs one before verifying, so the
 * harness must too. (The argument is a FUNCTION POINTER, not an epoch.) */
extern int mbedtls_x509_set_time(long (*fn)(void)) __attribute__((weak));

static long fixed_now(void)
{
    const char *e = getenv("HARNESS_NOW");
    return e ? atol(e) : 1793145600L; /* default 2026-10-28T00:00:00Z */
}

static unsigned char *slurp(const char *path, size_t *len)
{
    FILE *f = fopen(path, "rb");
    unsigned char *buf;
    long n;

    if (f == NULL) {
        perror(path);
        return NULL;
    }
    if (fseek(f, 0, SEEK_END) != 0) { fclose(f); return NULL; }
    n = ftell(f);
    if (n < 0) { fclose(f); return NULL; }
    rewind(f);
    buf = malloc((size_t) n + 1);
    if (buf == NULL) { fclose(f); return NULL; }
    if (fread(buf, 1, (size_t) n, f) != (size_t) n) { fclose(f); free(buf); return NULL; }
    fclose(f);
    buf[n] = 0;
    *len = (size_t) n;
    return buf;
}

/* The device build has MBEDTLS_ERROR_C off, so mbedtls_strerror() is empty.
 * Name the codes this harness can actually produce. */
static const char *errname(int ret)
{
    switch (-ret) {
    case 0:      return "OK";
    case 0x2180: return "X509_INVALID_FORMAT";
    case 0x2200: return "X509_INVALID_VERSION";
    case 0x2280: return "X509_INVALID_SERIAL";
    case 0x2300: return "X509_INVALID_ALG";
    case 0x2380: return "X509_INVALID_NAME";
    case 0x2400: return "X509_INVALID_TIME";
    case 0x2480: return "X509_INVALID_PUBKEY";
    case 0x2500: return "X509_INVALID_SIGNATURE";
    case 0x2580: return "X509_INVALID_EXTENSIONS";
    case 0x2600: return "X509_UNKNOWN_VERSION";
    case 0x2700: return "X509_CERT_VERIFY_FAILED";
    case 0x2800: return "X509_CRL_INVALID_FORMAT";
    case 0x0060: return "ASN1_OUT_OF_DATA";
    case 0x0062: return "ASN1_UNEXPECTED_TAG";
    case 0x0064: return "ASN1_INVALID_LENGTH";
    case 0x0066: return "ASN1_LENGTH_MISMATCH";
    case 0x0068: return "ASN1_INVALID_DATA";
    case 0x006a: return "ASN1_ALLOC_FAILED";
    case 0x006c: return "ASN1_BUF_TOO_SMALL";
    case 0x3d00: return "PK_INVALID_ALG";
    case 0x3e00: return "PK_UNKNOWN_OID";
    case 0x3f00: return "PK_INVALID_PUBKEY";
    default:     return "?";
    }
}

static void report(const char *what, int ret)
{
    printf("%s: -0x%04x (%s)\n", what, (unsigned) -ret, errname(ret));
}

int main(int argc, char **argv)
{
    const char *mode;
    unsigned char *der = NULL, *cader = NULL;
    size_t derlen = 0, calen = 0;
    mbedtls_x509_crt crt, ca;
    uint32_t flags = 0;
    int ret;
    char *text;
    size_t tlen = 256;

    if (argc < 3) {
        fprintf(stderr, "usage: %s <mode> <der> [<ca.der>]\n", argv[0]);
        return 3;
    }
    mode = argv[1];

    der = slurp(argv[2], &derlen);
    if (der == NULL) return 3;
    if (argc > 3 && strcmp(argv[3], "-") != 0) {
        cader = slurp(argv[3], &calen);
        if (cader == NULL) return 3;
    }

    if (mbedtls_x509_set_time != NULL && getenv("HARNESS_NO_TIME") == NULL)
        mbedtls_x509_set_time(fixed_now);
    mbedtls_x509_crt_init(&crt);
    mbedtls_x509_crt_init(&ca);

    if (strcmp(mode, "parse") == 0) {
        ret = mbedtls_x509_crt_parse_der(&crt, der, derlen);
        report("parse_der", ret);
        mbedtls_x509_crt_free(&crt);
        return ret == 0 ? 0 : 1;
    }

    if (strcmp(mode, "nocopy") == 0) {
        ret = mbedtls_x509_crt_parse_der_nocopy(&crt, der, derlen);
        report("parse_der_nocopy", ret);
        mbedtls_x509_crt_free(&crt);
        return ret == 0 ? 0 : 1;
    }

    /* everything below needs a parsed chain */
    ret = mbedtls_x509_crt_parse_der_nocopy(&crt, der, derlen);
    if (ret != 0) {
        report("parse_der_nocopy", ret);
        mbedtls_x509_crt_free(&crt);
        return 1;
    }

    if (strcmp(mode, "dn") == 0) {
        text = malloc(tlen);
        if (text == NULL) return 4;
        ret = mbedtls_x509_dn_gets(text, tlen, &crt.subject);
        report("dn_gets(subject)", ret);
        printf("subject: %s\n", text);
        ret = mbedtls_x509_dn_gets(text, tlen, &crt.issuer);
        report("dn_gets(issuer)", ret);
        printf("issuer: %s\n", text);
        free(text);
        mbedtls_x509_crt_free(&crt);
        return 0;
    }

    if (strcmp(mode, "info") == 0) {
        text = malloc(8192);
        if (text == NULL) return 4;
        ret = mbedtls_x509_crt_info(text, 8192, "", &crt);
        report("crt_info", ret);
        if (ret > 0) fwrite(text, 1, (size_t) ret, stdout);
        free(text);
        mbedtls_x509_crt_free(&crt);
        return 0;
    }

    if (strcmp(mode, "chain") == 0) {
        if (cader == NULL) { fprintf(stderr, "chain needs a CA\n"); return 3; }
        ret = mbedtls_x509_crt_parse_der_nocopy(&ca, cader, calen);
        if (ret != 0) {
            report("parse_der_nocopy(ca)", ret);
            mbedtls_x509_crt_free(&crt);
            mbedtls_x509_crt_free(&ca);
            return 1;
        }
    } else {
        /* self-signed: trust the cert itself */
        ret = mbedtls_x509_crt_parse_der_nocopy(&ca, der, derlen);
        if (ret != 0) {
            report("parse_der_nocopy(trust)", ret);
            mbedtls_x509_crt_free(&crt);
            mbedtls_x509_crt_free(&ca);
            return 1;
        }
    }

    ret = mbedtls_x509_crt_verify_with_profile(&crt, &ca, NULL,
                                               &mbedtls_x509_crt_profile_default,
                                               (argc > 4 && strcmp(argv[4], "-") != 0) ? argv[4] : (argc > 4 ? NULL : "san-test"),
                                               &flags, NULL, NULL);
    report("verify_with_profile", ret);
    printf("flags: 0x%08x\n", (unsigned) flags);

    mbedtls_x509_crt_free(&crt);
    mbedtls_x509_crt_free(&ca);
    return ret == 0 ? 0 : 2;
}
