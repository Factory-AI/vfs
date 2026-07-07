#define _GNU_SOURCE
#include "test-common.h"
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>

int test_link(const char *base_path) {
    char path[512], link_path[512], link_path2[512];
    struct stat st_orig, st_link;
    int result, fd;

    snprintf(path, sizeof(path), "%s/test.txt", base_path);
    snprintf(link_path, sizeof(link_path), "%s/test_hardlink", base_path);
    snprintf(link_path2, sizeof(link_path2), "%s/test_hardlink2", base_path);

    /* Clean up any previous test files */
    unlink(link_path);
    unlink(link_path2);

    /* Test 1: Create a hard link to an existing file */
    result = link(path, link_path);

    /* Skip link tests if link syscall is not supported */
    if (result < 0 && (errno == ENOSYS || errno == EOPNOTSUPP)) {
        printf("  (Skipping hard link tests - syscall not supported)\n");
        return 0;
    }
    TEST_ASSERT_ERRNO(result == 0, "link creation should succeed");

    /* Test 2: Verify hard link shares the same inode */
    result = stat(path, &st_orig);
    TEST_ASSERT_ERRNO(result == 0, "stat on original should succeed");

    result = stat(link_path, &st_link);
    TEST_ASSERT_ERRNO(result == 0, "stat on hard link should succeed");

    TEST_ASSERT(st_orig.st_ino == st_link.st_ino, "hard link should share inode with original");
    TEST_ASSERT(S_ISREG(st_link.st_mode), "hard link should be a regular file");

    /* Test 3: Verify link count is correct (at least 2) */
    TEST_ASSERT(st_link.st_nlink >= 2, "nlink should be at least 2 after creating hard link");

    /* Test 4: Verify data is shared - write through hard link, read from original */
    fd = open(link_path, O_WRONLY | O_TRUNC);
    TEST_ASSERT_ERRNO(fd >= 0, "open hard link for writing should succeed");
    result = write(fd, "modified", 8);
    TEST_ASSERT_ERRNO(result == 8, "write through hard link should succeed");
    close(fd);

    /* Read from original file */
    char buf[16] = {0};
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open original file for reading should succeed");
    result = read(fd, buf, sizeof(buf) - 1);
    TEST_ASSERT_ERRNO(result == 8, "read from original should succeed");
    TEST_ASSERT(memcmp(buf, "modified", 8) == 0, "data written via hard link should be visible in original");
    close(fd);

    /* Test 5: Create another hard link */
    result = link(path, link_path2);
    TEST_ASSERT_ERRNO(result == 0, "creating second hard link should succeed");

    result = stat(path, &st_orig);
    TEST_ASSERT_ERRNO(result == 0, "stat on original after second link should succeed");
    TEST_ASSERT(st_orig.st_nlink >= 3, "nlink should be at least 3 after second hard link");

    /* Test 6: Remove one hard link, verify others still work */
    result = unlink(link_path);
    TEST_ASSERT_ERRNO(result == 0, "unlink first hard link should succeed");

    result = stat(path, &st_orig);
    TEST_ASSERT_ERRNO(result == 0, "original should still exist after unlinking hard link");
    TEST_ASSERT(st_orig.st_nlink >= 2, "nlink should be at least 2 after removing one link");

    /* Test 7: link to non-existent file should fail */
    result = link("/nonexistent/file", link_path);
    TEST_ASSERT(result < 0, "link to non-existent file should fail");
    TEST_ASSERT(errno == ENOENT, "errno should be ENOENT for non-existent source");

    /* Test 8: link to existing destination should fail */
    result = link(path, link_path2);
    TEST_ASSERT(result < 0, "link to existing destination should fail");
    TEST_ASSERT(errno == EEXIST, "errno should be EEXIST for existing destination");

    /* Test 9: Verify hard link to directory fails (if we have a directory to test with) */
    char dir_path[512];
    snprintf(dir_path, sizeof(dir_path), "%s/subdir", base_path);

    /* Try to create the directory if it doesn't exist */
    mkdir(dir_path, 0755);

    result = link(dir_path, link_path);
    if (result < 0) {
        TEST_ASSERT(errno == EPERM || errno == EISDIR || errno == ENOENT,
                   "link to directory should fail with EPERM, EISDIR, or ENOENT");
    }

    /* Clean up */
    unlink(link_path2);

    /* Restore original file content for other tests */
    fd = open(path, O_WRONLY | O_TRUNC);
    if (fd >= 0) {
        write(fd, "test content\n", 13);
        close(fd);
    }

    return 0;
}
