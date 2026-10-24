#ifndef _STUB_STDIO_H
#define _STUB_STDIO_H
#include <stddef.h>
typedef struct _FILE FILE;
extern FILE *stderr;
extern FILE *stdout;
#define SEEK_END 2
FILE *fopen(const char *, const char *);
size_t fread(void *, size_t, size_t, FILE *);
int fseek(FILE *, long, int);
long ftell(FILE *);
void rewind(FILE *);
int fclose(FILE *);
int printf(const char *, ...);
int fprintf(FILE *, const char *, ...);
int fflush(FILE *);
#endif
