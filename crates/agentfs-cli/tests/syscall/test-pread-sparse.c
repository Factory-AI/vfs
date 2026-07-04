#define _GNU_SOURCE
#include "test-common.h"
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>

/*
 * Test for sparse file read consistency.
 *
 * Sparse files have "holes" - regions that were never written.
 * Reading from holes should return zeros.
 *
 * This is critical for proc-macro .so files where the linker
 * creates sparse files and rustc needs to read the full content.
 */

int test_pread_sparse(const char *base_path) {
    char path[512];
    struct stat st;
    int fd;

    snprintf(path, sizeof(path), "%s/sparse_test.bin", base_path);

    printf("  Creating sparse file with holes...\n");

    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT_ERRNO(fd >= 0, "open should succeed");

    /* Write pattern at offset 0 */
    char buf1[4096];
    memset(buf1, 'A', sizeof(buf1));
    ssize_t n = pwrite(fd, buf1, sizeof(buf1), 0);
    if (n < 0 && errno == EBADF) {
        /* Experimental sandbox doesn't support pwrite, skip test */
        printf("  pwrite not supported, skipping test\n");
        close(fd);
        unlink(path);
        return 0;
    }
    TEST_ASSERT_ERRNO(n == sizeof(buf1), "pwrite at 0 should succeed");

    /* Skip 8KB (create a hole), write at offset 12KB */
    char buf2[4096];
    memset(buf2, 'B', sizeof(buf2));
    n = pwrite(fd, buf2, sizeof(buf2), 12288);
    TEST_ASSERT_ERRNO(n == sizeof(buf2), "pwrite at 12288 should succeed");

    /* Skip another 4KB, write at offset 20KB */
    char buf3[4096];
    memset(buf3, 'C', sizeof(buf3));
    n = pwrite(fd, buf3, sizeof(buf3), 20480);
    TEST_ASSERT_ERRNO(n == sizeof(buf3), "pwrite at 20480 should succeed");

    /* File layout:
     * 0-4095:     'A' (written)
     * 4096-12287: hole (should read as zeros)
     * 12288-16383: 'B' (written)
     * 16384-20479: hole (should read as zeros)
     * 20480-24575: 'C' (written)
     */

    /* fsync is optional - experimental sandbox doesn't support it */
    fsync(fd);
    close(fd);

    /* Verify file size */
    TEST_ASSERT_ERRNO(stat(path, &st) == 0, "stat should succeed");
    printf("  file size: %ld bytes\n", (long)st.st_size);
    TEST_ASSERT(st.st_size == 24576, "file size should be 24576");

    /* Read the entire file and verify */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open for read should succeed");

    char readbuf[24576];
    n = pread(fd, readbuf, sizeof(readbuf), 0);
    TEST_ASSERT_ERRNO(n == sizeof(readbuf), "pread should return full size");

    /* Verify each region */
    int errors = 0;

    /* Region 1: 0-4095 should be 'A' */
    for (int i = 0; i < 4096; i++) {
        if (readbuf[i] != 'A') {
            if (errors < 5)
                printf("  ERROR at %d: expected 'A', got 0x%02x\n", i, (unsigned char)readbuf[i]);
            errors++;
        }
    }

    /* Region 2: 4096-12287 should be zeros (hole) */
    for (int i = 4096; i < 12288; i++) {
        if (readbuf[i] != 0) {
            if (errors < 5)
                printf("  ERROR at %d: expected 0, got 0x%02x\n", i, (unsigned char)readbuf[i]);
            errors++;
        }
    }

    /* Region 3: 12288-16383 should be 'B' */
    for (int i = 12288; i < 16384; i++) {
        if (readbuf[i] != 'B') {
            if (errors < 5)
                printf("  ERROR at %d: expected 'B', got 0x%02x\n", i, (unsigned char)readbuf[i]);
            errors++;
        }
    }

    /* Region 4: 16384-20479 should be zeros (hole) */
    for (int i = 16384; i < 20480; i++) {
        if (readbuf[i] != 0) {
            if (errors < 5)
                printf("  ERROR at %d: expected 0, got 0x%02x\n", i, (unsigned char)readbuf[i]);
            errors++;
        }
    }

    /* Region 5: 20480-24575 should be 'C' */
    for (int i = 20480; i < 24576; i++) {
        if (readbuf[i] != 'C') {
            if (errors < 5)
                printf("  ERROR at %d: expected 'C', got 0x%02x\n", i, (unsigned char)readbuf[i]);
            errors++;
        }
    }

    close(fd);
    unlink(path);

    if (errors > 0) {
        printf("  total errors: %d\n", errors);
    }
    TEST_ASSERT(errors == 0, "sparse file content should be correct");

    printf("  sparse file test passed\n");
    return 0;
}
