#ifndef _STUB_PTHREAD_H
#define _STUB_PTHREAD_H
typedef unsigned long pthread_t; typedef union { char b[40]; long a; } pthread_mutex_t;
int pthread_mutex_init(pthread_mutex_t *, const void *);
int pthread_mutex_lock(pthread_mutex_t *); int pthread_mutex_unlock(pthread_mutex_t *);
int pthread_mutex_destroy(pthread_mutex_t *);
#endif
