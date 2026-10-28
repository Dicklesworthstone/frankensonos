/* d7vi harness: feed a .ogg through libvorbis 1.3.7's real decode path
 * (vorbis_synthesis_headerin + vorbis_synthesis + vorbis_synthesis_blockin,
 * the same sequence anacapad runs), under ASan. One file per run so a crash
 * is attributable; the runner enforces verdicts.
 *
 * usage: vorbis_decode_asan <file.ogg>
 * exit 0 = clean decode/decode-error; ASan aborts nonzero on memory bugs.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vorbis/codec.h>
#include <ogg/ogg.h>

static long slurp(const char *p, unsigned char **out) {
    FILE *f = fopen(p, "rb");
    if (!f) { perror("open"); exit(2); }
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (n > 64 * 1024 * 1024) n = 64 * 1024 * 1024; /* cap */
    *out = malloc(n ? n : 1);
    long got = (long)fread(*out, 1, n, f);
    fclose(f);
    return got;
}

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "usage: %s file.ogg\n", argv[0]); return 2; }
    unsigned char *buf;
    long n = slurp(argv[1], &buf);

    ogg_sync_state oy; ogg_stream_state os; ogg_page og;
    vorbis_info vi; vorbis_comment vc; vorbis_dsp_state vd;
    vorbis_block vb;
    int block_inited = 0;

    ogg_sync_init(&oy); vorbis_info_init(&vi); vorbis_comment_init(&vc);
    int headers = 0, in_stream = 0, packetno = 0;
    char *ptr = (char *)buf;
    long remaining = n;
    int guard = 0;

    while (remaining > 0 && guard < 100000) {
        guard++;
        long size = remaining < 65536 ? remaining : 65536;
        char *buffer = ogg_sync_buffer(&oy, (long)size);
        memcpy(buffer, ptr, size);
        ogg_sync_wrote(&oy, size);
        ptr += size; remaining -= size;

        while (ogg_sync_pageout(&oy, &og) > 0) {
            if (!in_stream) {
                ogg_stream_init(&os, ogg_page_serialno(&og));
                in_stream = 1;
            }
            if (ogg_page_serialno(&og) != os.serialno) continue; /* first chain only */
            ogg_stream_pagein(&os, &og);
            ogg_packet op;
            while (ogg_stream_packetout(&os, &op) > 0) {
                if (headers < 3) {
                    int r = vorbis_synthesis_headerin(&vi, &vc, &op);
                    if (r != 0) { headers = 99; goto decode_done; } /* stop chain */
                    headers++;
                    if (headers == 3) {
                        if (vorbis_synthesis_init(&vd, &vi) != 0) goto decode_done;
                        vorbis_block_init(&vd, &vb);
                        block_inited = 1;
                    }
                } else {
                    int r = vorbis_synthesis(&vb, &op);
                    if (r == 0) vorbis_synthesis_blockin(&vd, &vb);
                    float **pcm;
                    for (;;) {
                        int avail = vorbis_synthesis_pcmout(&vd, &pcm);
                        if (avail <= 0) break;
                        vorbis_synthesis_read(&vd, avail);
                    }
                    (void)r;
                }
                packetno++;
            }
        }
    }
decode_done:
    if (block_inited) { vorbis_block_clear(&vb); vorbis_dsp_clear(&vd); }
    if (in_stream) ogg_stream_clear(&os);
    ogg_sync_clear(&oy);
    vorbis_comment_clear(&vc); vorbis_info_init(&vi); /* reinit not needed; clear below */
    vorbis_info_clear(&vi);
    free(buf);
    fprintf(stderr, "clean packets=%d headers=%d\n", packetno, headers);
    return 0;
}
