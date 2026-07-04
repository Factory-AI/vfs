/*
 * perf-statx.c - Micro-benchmark for the statx() system call
 *
 * Usage: ./perf-statx <file> [iterations]
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <linux/stat.h>

#define DEFAULT_ITERATIONS 100000
#define WARMUP_ITERATIONS  1000

static inline long long timespec_to_ns(struct timespec *ts)
{
    return (long long)ts->tv_sec * 1000000000LL + ts->tv_nsec;
}

static inline long long get_time_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return timespec_to_ns(&ts);
}

int main(int argc, char *argv[])
{
    struct statx stx;
    const char *path;
    int iterations;
    long long start, end, elapsed;
    double avg_ns;
    int i, ret;

    if (argc < 2) {
        fprintf(stderr, "Usage: %s <file> [iterations]\n", argv[0]);
        return 1;
    }

    path = argv[1];
    iterations = (argc >= 3) ? atoi(argv[2]) : DEFAULT_ITERATIONS;

    if (iterations <= 0) {
        fprintf(stderr, "Invalid iteration count\n");
        return 1;
    }

    /* Verify file exists */
    ret = syscall(SYS_statx, AT_FDCWD, path, 0, STATX_BASIC_STATS, &stx);
    if (ret < 0) {
        perror("statx");
        return 1;
    }

    /* Warmup */
    for (i = 0; i < WARMUP_ITERATIONS; i++) {
        syscall(SYS_statx, AT_FDCWD, path, 0, STATX_BASIC_STATS, &stx);
    }

    /* Benchmark */
    start = get_time_ns();
    for (i = 0; i < iterations; i++) {
        syscall(SYS_statx, AT_FDCWD, path, 0, STATX_BASIC_STATS, &stx);
    }
    end = get_time_ns();

    elapsed = end - start;
    avg_ns = (double)elapsed / iterations;

    printf("statx() micro-benchmark\n");
    printf("-----------------------\n");
    printf("File:        %s\n", path);
    printf("Iterations:  %d\n", iterations);
    printf("Total time:  %.3f ms\n", elapsed / 1000000.0);
    printf("Avg latency: %.1f ns\n", avg_ns);
    printf("Throughput:  %.0f ops/sec\n", 1000000000.0 / avg_ns);

    return 0;
}
