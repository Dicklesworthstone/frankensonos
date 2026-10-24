#ifndef _STUB_STDLIB_H
#define _STUB_STDLIB_H
#include <stddef.h>
void *malloc(size_t); void *calloc(size_t, size_t); void free(void *);
int atoi(const char *); void abort(void); void exit(int);
#endif
