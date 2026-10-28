/* Minimal C decode harness: feed whole file via one read callback, decode all,
   ASan-instrumented. Mirrors anacapad usage (stream decoder, full decode). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "FLAC/stream_decoder.h"

static FILE *f;
static FLAC__StreamDecoderReadStatus rd(FLAC__byte buf[], size_t *bytes, void *cd) {
    (void)cd;
    size_t n = fread(buf, 1, *bytes, f);
    *bytes = n;
    if (n == 0) return feof(f) ? FLAC__STREAM_DECODER_READ_STATUS_END_OF_STREAM
                               : FLAC__STREAM_DECODER_READ_STATUS_ABORT;
    return FLAC__STREAM_DECODER_READ_STATUS_CONTINUE;
}
static FLAC__StreamDecoderWriteStatus wr(const FLAC__StreamDecoder *d, const FLAC__Frame *fr,
                                         const FLAC__int32 *const buf[], void *cd) {
    (void)d; (void)cd;
    /* touch the output like a consumer would */
    volatile int32_t s = 0;
    for (unsigned ch = 0; ch < fr->header.channels; ch++)
        for (unsigned i = 0; i < fr->header.blocksize; i++) s += buf[ch][i];
    return FLAC__STREAM_DECODER_WRITE_STATUS_CONTINUE;
}
static void errcb(const FLAC__StreamDecoder *d, FLAC__StreamDecoderErrorStatus st, void *cd) {
    (void)d; (void)cd; (void)st;
}
int main(int argc, char **argv) {
    f = fopen(argv[1], "rb");
    if (!f) return 2;
    FLAC__StreamDecoder *d = FLAC__stream_decoder_new();
    FLAC__stream_decoder_init_stream(d, rd, NULL, NULL, NULL, NULL, wr, NULL, errcb);
    FLAC__stream_decoder_process_until_end_of_stream(d);
    FLAC__StreamDecoderState st = FLAC__stream_decoder_get_state(d);
    fprintf(stderr, "final state: %s\n", FLAC__stream_decoder_get_resolved_state_string(d));
    FLAC__stream_decoder_delete(d);
    fclose(f);
    return st == FLAC__STREAM_DECODER_MEMORY_ALLOCATION_ERROR ? 3 : 0;
}
