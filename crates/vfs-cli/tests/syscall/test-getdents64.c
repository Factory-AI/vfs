#define _GNU_SOURCE
#include "test-common.h"
#include <sys/syscall.h>
#include <fcntl.h>
#include <unistd.h>
#include <dirent.h>

/* Linux dirent64 structure */
struct linux_dirent64 {
    unsigned long  d_ino;
    unsigned long  d_off;
    unsigned short d_reclen;
    unsigned char  d_type;
    char           d_name[];
};

int test_getdents64(const char *base_path) {
    int fd, nread;
    char buf[4096];  /* Larger buffer to handle more directory entries */
    struct linux_dirent64 *d;
    int found_test = 0;
    int total_entries = 0;

    /* Test 1: Open directory */
    fd = open(base_path, O_RDONLY | O_DIRECTORY);
    TEST_ASSERT_ERRNO(fd >= 0, "open directory should succeed");

    /* Test 2: Call getdents64 - loop until all entries are read */
    while ((nread = syscall(SYS_getdents64, fd, buf, sizeof(buf))) > 0) {
        /* Test 3: Parse directory entries */
        for (int pos = 0; pos < nread;) {
            d = (struct linux_dirent64 *) (buf + pos);
            total_entries++;

            if (strcmp(d->d_name, "test.txt") == 0) {
                found_test = 1;
                TEST_ASSERT(d->d_type == DT_REG, "test.txt should be a regular file");
            }

            pos += d->d_reclen;
        }
    }
    TEST_ASSERT_ERRNO(nread == 0, "getdents64 should return 0 at end of directory");
    TEST_ASSERT(total_entries > 0, "getdents64 should return at least one entry");

    TEST_ASSERT(found_test, "should find test.txt in directory listing");

    close(fd);

    /* Test 4: getdents64 on closed fd should fail */
    nread = syscall(SYS_getdents64, fd, buf, sizeof(buf));
    TEST_ASSERT(nread < 0 && errno == EBADF, "getdents64 on closed fd should fail with EBADF");

    /* Test 5: getdents64 on regular file should fail */
    char path[512];
    snprintf(path, sizeof(path), "%s/test.txt", base_path);
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open file should succeed");

    nread = syscall(SYS_getdents64, fd, buf, sizeof(buf));
    TEST_ASSERT(nread < 0 && errno == ENOTDIR, "getdents64 on regular file should fail with ENOTDIR");

    close(fd);

    return 0;
}
