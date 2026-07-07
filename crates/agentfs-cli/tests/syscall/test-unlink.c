#define _GNU_SOURCE
#include "test-common.h"
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>

int test_unlink(const char *base_path) {
    char path[512], link_path[512];
    struct stat st;
    int result, fd;

    snprintf(path, sizeof(path), "%s/unlink_test.txt", base_path);
    snprintf(link_path, sizeof(link_path), "%s/unlink_test_link", base_path);

    /* Clean up any previous test files */
    unlink(link_path);
    unlink(path);

    /* Test 1: Create a file */
    fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT_ERRNO(fd >= 0, "create file should succeed");
    result = write(fd, "test data", 9);
    TEST_ASSERT_ERRNO(result == 9, "write should succeed");
    close(fd);

    /* Test 2: Create a hard link */
    result = link(path, link_path);
    if (result < 0 && (errno == ENOSYS || errno == EOPNOTSUPP)) {
        printf("  (Skipping unlink path cache test - hard links not supported)\n");
        unlink(path);
        return 0;
    }
    TEST_ASSERT_ERRNO(result == 0, "link creation should succeed");

    /* Test 3: Unlink the original file */
    result = unlink(path);
    TEST_ASSERT_ERRNO(result == 0, "unlink original should succeed");

    /* Test 4: Access the hard link - this triggers the bug fixed in da06605
     * Before the fix, the path cache was invalidated when unlinking,
     * even though the hard link still references the same inode.
     */
    result = stat(link_path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on remaining hard link should succeed after unlink");
    TEST_ASSERT(S_ISREG(st.st_mode), "hard link should still be a regular file");
    TEST_ASSERT(st.st_nlink == 1, "nlink should be 1 after removing original");

    /* Test 5: Read from the hard link */
    char buf[16] = {0};
    fd = open(link_path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open hard link for reading should succeed");
    result = read(fd, buf, sizeof(buf) - 1);
    TEST_ASSERT_ERRNO(result == 9, "read from hard link should succeed");
    TEST_ASSERT(memcmp(buf, "test data", 9) == 0, "data should be intact via hard link");
    close(fd);

    /* Test 6: Write to the hard link */
    fd = open(link_path, O_WRONLY | O_TRUNC);
    TEST_ASSERT_ERRNO(fd >= 0, "open hard link for writing should succeed");
    result = write(fd, "new data", 8);
    TEST_ASSERT_ERRNO(result == 8, "write to hard link should succeed");
    close(fd);

    /* Clean up */
    unlink(link_path);

    return 0;
}
