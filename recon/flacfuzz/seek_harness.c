/* Decode + seek harness: mimics a player seeking to absolute sample N. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "FLAC/stream_decoder.h"

static FILE *f;
static FLAC__StreamDecoderReadStatus rd(const FLAC__StreamDecoder *d, FLAC__byte buf[], size_t *bytes, void *cd) {
    (void)d; (void)cd;
    size_t n = fread(buf, 1, *bytes, f);
    *bytes = n;
    if (n == 0) return feof(f) ? FLAC__STREAM_DECODER_READ_STATUS_END_OF_STREAM : FLAC__STREAM_DECODER_READ_STATUS_ABORT;
    return FLAC__STREAM_DECODER_READ_STATUS_CONTINUE;
}
static FLAC__StreamDecoderSeekStatus sk(const FLAC__StreamDecoder *d, FLAC__uint64 off, void *cd) {
    (void)d; (void)cd;
    return fseeko(f, (off_t)off, SEEK_SET) == 0 ? FLAC__STREAM_DECODER_SEEK_STATUS_OK : FLAC__STREAM_DECODER_SEEK_STATUS_ERROR;
}
static FLAC__StreamDecoderTellStatus tl(const FLAC__StreamDecoder *d, FLAC__uint64 *off, void *cd) {
    (void)d; (void)cd;
    off_t p = ftello(f);
    *off = (FLAC__uint64)p;
    return p >= 0 ? FLAC__STREAM_DECODER_TELL_STATUS_OK : FLAC__STREAM_DECODER_TELL_STATUS_ERROR;
}
static FLAC__StreamDecoderLengthStatus ln(const FLAC__StreamDecoder *d, FLAC__uint64 *len, void *cd) {
    (void)d; (void)cd;
    long cur = ftello(f); fseeko(f, 0, SEEK_END); long e = ftello(f); fseeko(f, cur, SEEK_SET);
    *len = (FLAC__uint64)e;
    return FLAC__STREAM_DECODER_LENGTH_STATUS_OK;
}
static FLAC__bool eofcb(const FLAC__StreamDecoder *d, void *cd) { (void)d; (void)cd; return !!feof(f); }
static FLAC__StreamDecoderWriteStatus wr(const FLAC__StreamDecoder *d, const FLAC__Frame *fr, const FLAC__int32 *const buf[], void *cd) {
    (void)d; (void)cd;
    fprintf(stderr, "wrote ch=%u bs=%u sn=%llu\n", fr->header.channels, fr->header.blocksize,
            (unsigned long long)fr->header.number.sample_number);
    return FLAC__STREAM_DECODER_WRITE_STATUS_CONTINUE;
}
static void errcb(const FLAC__StreamDecoder *d, FLAC__StreamDecoderErrorStatus st, void *cd) {
    (void)d; (void)cd; fprintf(stderr, "  err:%d\n", st);
}
int main(int argc, char **argv) {
    f = fopen(argv[1], "rb");
    if (!f) return 2;
    FLAC__uint64 target = strtoull(argv[2], 0, 0);
    FLAC__StreamDecoder *d = FLAC__stream_decoder_new();
    FLAC__stream_decoder_init_stream(d, rd, sk, tl, ln, eofcb, wr, NULL, errcb, NULL);
    FLAC__stream_decoder_process_until_end_of_metadata(d);
    fprintf(stderr, "seek to %llu\n", (unsigned long long)target);
    FLAC__bool ok = FLAC__stream_decoder_seek_absolute(d, target);
    fprintf(stderr, "seek ok=%d state=%s\n", ok, FLAC__stream_decoder_get_resolved_state_string(d));
    FLAC__stream_decoder_process_single(d);
    fprintf(stderr, "final: %s\n", FLAC__stream_decoder_get_resolved_state_string(d));
    FLAC__stream_decoder_delete(d);
    return 0;
}
