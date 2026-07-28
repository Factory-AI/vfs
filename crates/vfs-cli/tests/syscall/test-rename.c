#define _GNU_SOURCE
#include "test-common.h"
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>

int test_rename(const char *base_path) {
    char src_path[512], dst_path[512];
    struct stat st_src, st_dst;
    int result, fd;

    snprintf(src_path, sizeof(src_path), "%s/rename_src.txt", base_path);
    snprintf(dst_path, sizeof(dst_path), "%s/rename_dst.txt", base_path);

    /* Clean up any previous test files */
    unlink(src_path);
    unlink(dst_path);

    /* Test 1: Create a source file */
    fd = open(src_path, O_CREAT | O_WRONLY, 0644);
    TEST_ASSERT_ERRNO(fd >= 0, "create source file should succeed");
    result = write(fd, "rename test", 11);
    TEST_ASSERT_ERRNO(result == 11, "write to source should succeed");
    close(fd);

    /* Get inode of source before rename */
    result = stat(src_path, &st_src);
    TEST_ASSERT_ERRNO(result == 0, "stat source before rename should succeed");

    /* Test 2: Rename to non-existent destination (this is the bug!) */
    result = rename(src_path, dst_path);
    TEST_ASSERT_ERRNO(result == 0, "rename to non-existent destination should succeed");

    /* Verify source no longer exists */
    result = stat(src_path, &st_src);
    TEST_ASSERT(result < 0 && errno == ENOENT, "source should not exist after rename");

    /* Verify destination exists with same inode */
    result = stat(dst_path, &st_dst);
    TEST_ASSERT_ERRNO(result == 0, "destination should exist after rename");

    /* Verify content is preserved */
    char buf[32] = {0};
    fd = open(dst_path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open renamed file should succeed");
    result = read(fd, buf, sizeof(buf) - 1);
    TEST_ASSERT_ERRNO(result == 11, "read from renamed file should succeed");
    TEST_ASSERT(memcmp(buf, "rename test", 11) == 0, "content should be preserved after rename");
    close(fd);

    /* Test 3: Rename to existing destination (should replace) */
    snprintf(src_path, sizeof(src_path), "%s/rename_src2.txt", base_path);
    unlink(src_path);

    fd = open(src_path, O_CREAT | O_WRONLY, 0644);
    TEST_ASSERT_ERRNO(fd >= 0, "create second source file should succeed");
    result = write(fd, "new content", 11);
    TEST_ASSERT_ERRNO(result == 11, "write to second source should succeed");
    close(fd);

    result = rename(src_path, dst_path);
    TEST_ASSERT_ERRNO(result == 0, "rename to existing destination should succeed");

    /* Verify new content */
    memset(buf, 0, sizeof(buf));
    fd = open(dst_path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open replaced file should succeed");
    result = read(fd, buf, sizeof(buf) - 1);
    TEST_ASSERT_ERRNO(result == 11, "read from replaced file should succeed");
    TEST_ASSERT(memcmp(buf, "new content", 11) == 0, "content should be updated after replace");
    close(fd);

    /* Test 4: Rename non-existent source should fail with ENOENT */
    result = rename("/nonexistent/file", dst_path);
    TEST_ASSERT(result < 0, "rename non-existent source should fail");
    TEST_ASSERT(errno == ENOENT, "errno should be ENOENT for non-existent source");

    /* Test 5: Rename directory */
    char src_dir[512], dst_dir[512];
    snprintf(src_dir, sizeof(src_dir), "%s/rename_dir_src", base_path);
    snprintf(dst_dir, sizeof(dst_dir), "%s/rename_dir_dst", base_path);

    /* Clean up */
    rmdir(dst_dir);
    rmdir(src_dir);

    result = mkdir(src_dir, 0755);
    TEST_ASSERT_ERRNO(result == 0, "create source directory should succeed");

    result = rename(src_dir, dst_dir);
    TEST_ASSERT_ERRNO(result == 0, "rename directory should succeed");

    result = stat(src_dir, &st_src);
    TEST_ASSERT(result < 0 && errno == ENOENT, "source dir should not exist after rename");

    result = stat(dst_dir, &st_dst);
    TEST_ASSERT_ERRNO(result == 0, "destination dir should exist after rename");
    TEST_ASSERT(S_ISDIR(st_dst.st_mode), "renamed entry should still be a directory");

    /* Clean up */
    rmdir(dst_dir);
    unlink(dst_path);

    return 0;
}
