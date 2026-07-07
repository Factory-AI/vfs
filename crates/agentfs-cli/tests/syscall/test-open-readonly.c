#define _GNU_SOURCE
#include "test-common.h"
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>

int test_open_readonly(const char *base_path) {
    char path[512];
    char buf[256];
    int fd;
    ssize_t n;

    snprintf(path, sizeof(path), "%s/readonly.txt", base_path);

    /* Test 1: Open read-only file (mode 0444) with O_RDONLY should succeed */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open(O_RDONLY) on read-only file should succeed");

    n = read(fd, buf, sizeof(buf) - 1);
    TEST_ASSERT_ERRNO(n > 0, "read from read-only file should return positive bytes");
    buf[n] = '\0';

    TEST_ASSERT(strncmp(buf, "readonly content", 16) == 0,
                "read-only file should contain expected content");
    close(fd);

    /* Test 2: Open read-only file (mode 0444) with O_RDWR should fail with EACCES */
    fd = open(path, O_RDWR);
    TEST_ASSERT(fd < 0, "open(O_RDWR) on read-only file should fail");
    TEST_ASSERT(errno == EACCES, "open(O_RDWR) on read-only file should fail with EACCES");

    /* Test 3: Open read-only file (mode 0444) with O_WRONLY should fail with EACCES */
    fd = open(path, O_WRONLY);
    TEST_ASSERT(fd < 0, "open(O_WRONLY) on read-only file should fail");
    TEST_ASSERT(errno == EACCES, "open(O_WRONLY) on read-only file should fail with EACCES");

    return 0;
}
