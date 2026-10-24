#ifndef _STUB_STRING_H
#define _STUB_STRING_H
#include <stddef.h>
void *memset(void *, int, size_t); void *memcpy(void *, const void *, size_t);
int strcmp(const char *, const char *); int strncmp(const char *, const char *, size_t);
char *strstr(const char *, const char *); size_t strlen(const char *);
long strtol(const char *, char **, int);
#endif
