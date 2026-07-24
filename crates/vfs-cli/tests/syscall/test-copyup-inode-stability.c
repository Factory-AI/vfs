#define _GNU_SOURCE
#include "test-common.h"
#include <sys/stat.h>
#include <sys/xattr.h>
#include <sys/time.h>
#include <fcntl.h>
#include <unistd.h>
#include <utime.h>

/**
 * Test for inode stability after copy-up in overlay filesystem.
 *
 * When a file is copied from the base layer to the delta layer (copy-up),
 * the kernel caches the original inode number. If we return a different
 * inode after copy-up, the kernel's cache becomes inconsistent, causing
 * ENOENT errors or other failures.
 *
 * This test verifies that inode numbers remain stable when copy-up is
 * triggered by various syscalls:
 *   - write() / pwrite() - writing to a file
 *   - truncate() / ftruncate() - changing file size
 *   - chmod() / fchmod() - changing permissions
 *   - chown() / fchown() - changing ownership
 *   - rename() - moving/renaming a file
 *   - link() - creating hard links
 *   - utimes() / utimensat() - changing timestamps
 *   - setxattr() - setting extended attributes
 *   - fallocate() - allocating file space
 *
 * Test setup (in test-run-syscalls.sh):
 *   Files named copyup_<syscall>_test.txt are created in the base layer
 *   before the overlay is mounted.
 *
 * Related to Linux overlayfs's trusted.overlay.origin mechanism.
 */

/*
 * Helper: Check that inode is stable after a copy-up operation.
 * Returns 0 on success, -1 on failure.
 */
static int check_inode_stable(const char *path, ino_t expected_ino, const char *op_name) {
    struct stat st;
    int result = stat(path, &st);
    if (result < 0) {
        fprintf(stderr, "  stat after %s failed: %s\n", op_name, strerror(errno));
        return -1;
    }
    if (st.st_ino != expected_ino) {
        fprintf(stderr, "  INODE CHANGED after %s: was %lu, now %lu\n",
                op_name, (unsigned long)expected_ino, (unsigned long)st.st_ino);
        return -1;
    }
    return 0;
}

/*
 * Helper: Check if a file exists in the base layer (skip test if not).
 * Returns the original inode, or 0 if the file doesn't exist.
 */
static ino_t get_base_layer_inode(const char *base_path, const char *filename, const char *test_name) {
    char path[512];
    struct stat st;

    snprintf(path, sizeof(path), "%s/%s", base_path, filename);
    if (stat(path, &st) < 0) {
        if (errno == ENOENT) {
            printf("  (Skipping %s test - %s not in base layer)\n", test_name, filename);
            return 0;
        }
        fprintf(stderr, "  stat on %s failed: %s\n", filename, strerror(errno));
        return 0;
    }
    return st.st_ino;
}

/**
 * Test 1: write() triggered copy-up
 *
 * Writing to a base layer file should trigger copy-up while preserving
 * the original inode number.
 */
static int test_write_copyup(const char *base_path) {
    char path[512];
    ino_t orig_ino;
    int fd, result;

    orig_ino = get_base_layer_inode(base_path, "copyup_write_test.txt", "write copyup");
    if (orig_ino == 0) return 0;

    snprintf(path, sizeof(path), "%s/copyup_write_test.txt", base_path);

    /* Open for writing and write data - this triggers copy-up */
    fd = open(path, O_WRONLY | O_APPEND);
    TEST_ASSERT_ERRNO(fd >= 0, "open for write should succeed");

    result = write(fd, " appended data", 14);
    TEST_ASSERT_ERRNO(result == 14, "write should succeed");
    close(fd);

    /* Verify inode is stable after copy-up */
    TEST_ASSERT(check_inode_stable(path, orig_ino, "write") == 0,
        "inode must remain stable after write copy-up");

    /* Also verify via fstat */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open after write should succeed");

    struct stat st;
    result = fstat(fd, &st);
    TEST_ASSERT_ERRNO(result == 0, "fstat after write should succeed");
    TEST_ASSERT(st.st_ino == orig_ino, "fstat must return stable inode after write copy-up");
    close(fd);

    return 0;
}

/**
 * Test 2: truncate() triggered copy-up
 *
 * Truncating a base layer file should trigger copy-up while preserving
 * the original inode number.
 */
static int test_truncate_copyup(const char *base_path) {
    char path[512];
    ino_t orig_ino;
    int result;

    orig_ino = get_base_layer_inode(base_path, "copyup_truncate_test.txt", "truncate copyup");
    if (orig_ino == 0) return 0;

    snprintf(path, sizeof(path), "%s/copyup_truncate_test.txt", base_path);

    /* Truncate the file - this triggers copy-up */
    result = truncate(path, 10);
    TEST_ASSERT_ERRNO(result == 0, "truncate should succeed");

    /* Verify inode is stable after copy-up */
    TEST_ASSERT(check_inode_stable(path, orig_ino, "truncate") == 0,
        "inode must remain stable after truncate copy-up");

    /* Also test ftruncate */
    int fd = open(path, O_WRONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open for ftruncate should succeed");

    result = ftruncate(fd, 5);
    TEST_ASSERT_ERRNO(result == 0, "ftruncate should succeed");

    struct stat st;
    result = fstat(fd, &st);
    TEST_ASSERT_ERRNO(result == 0, "fstat after ftruncate should succeed");
    TEST_ASSERT(st.st_ino == orig_ino, "fstat must return stable inode after ftruncate");
    close(fd);

    return 0;
}

/**
 * Test 3: chmod() triggered copy-up
 *
 * Changing permissions on a base layer file should trigger copy-up
 * while preserving the original inode number.
 */
static int test_chmod_copyup(const char *base_path) {
    char path[512];
    ino_t orig_ino;
    int result;

    orig_ino = get_base_layer_inode(base_path, "copyup_chmod_test.txt", "chmod copyup");
    if (orig_ino == 0) return 0;

    snprintf(path, sizeof(path), "%s/copyup_chmod_test.txt", base_path);

    /* chmod - this triggers copy-up */
    result = chmod(path, 0755);
    TEST_ASSERT_ERRNO(result == 0, "chmod should succeed");

    /* Verify inode is stable after copy-up */
    TEST_ASSERT(check_inode_stable(path, orig_ino, "chmod") == 0,
        "inode must remain stable after chmod copy-up");

    /* Also test fchmod */
    int fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open for fchmod should succeed");

    result = fchmod(fd, 0700);
    TEST_ASSERT_ERRNO(result == 0, "fchmod should succeed");

    struct stat st;
    result = fstat(fd, &st);
    TEST_ASSERT_ERRNO(result == 0, "fstat after fchmod should succeed");
    TEST_ASSERT(st.st_ino == orig_ino, "fstat must return stable inode after fchmod");
    close(fd);

    return 0;
}

/**
 * Test 4: chown() triggered copy-up
 *
 * Changing ownership on a base layer file should trigger copy-up
 * while preserving the original inode number.
 *
 * Note: This may fail without root privileges, which is expected.
 */
static int test_chown_copyup(const char *base_path) {
    char path[512];
    ino_t orig_ino;
    struct stat st;
    int result;

    orig_ino = get_base_layer_inode(base_path, "copyup_chown_test.txt", "chown copyup");
    if (orig_ino == 0) return 0;

    snprintf(path, sizeof(path), "%s/copyup_chown_test.txt", base_path);

    /* Get current owner */
    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat for chown should succeed");

    /* chown to same user (should still trigger copy-up) */
    result = chown(path, st.st_uid, st.st_gid);
    if (result < 0 && (errno == EPERM || errno == ENOSYS)) {
        printf("  (Skipping chown test - operation not permitted)\n");
        return 0;
    }
    TEST_ASSERT_ERRNO(result == 0, "chown should succeed");

    /* Verify inode is stable after copy-up */
    TEST_ASSERT(check_inode_stable(path, orig_ino, "chown") == 0,
        "inode must remain stable after chown copy-up");

    /* Also test lchown */
    result = lchown(path, st.st_uid, st.st_gid);
    TEST_ASSERT_ERRNO(result == 0, "lchown should succeed");
    TEST_ASSERT(check_inode_stable(path, orig_ino, "lchown") == 0,
        "inode must remain stable after lchown copy-up");

    return 0;
}

/**
 * Test 5: rename() triggered copy-up
 *
 * Renaming a base layer file should trigger copy-up while preserving
 * the original inode number (at the new path).
 */
static int test_rename_copyup(const char *base_path) {
    char orig_path[512], new_path[512];
    ino_t orig_ino;
    int result;

    orig_ino = get_base_layer_inode(base_path, "copyup_rename_test.txt", "rename copyup");
    if (orig_ino == 0) return 0;

    snprintf(orig_path, sizeof(orig_path), "%s/copyup_rename_test.txt", base_path);
    snprintf(new_path, sizeof(new_path), "%s/copyup_rename_test_renamed.txt", base_path);

    /* Clean up any previous renamed file */
    unlink(new_path);

    /* rename - this triggers copy-up */
    result = rename(orig_path, new_path);
    TEST_ASSERT_ERRNO(result == 0, "rename should succeed");

    /* The new path should have the same inode as the original */
    TEST_ASSERT(check_inode_stable(new_path, orig_ino, "rename") == 0,
        "inode must remain stable after rename copy-up");

    /* Original path should no longer exist */
    struct stat st;
    result = stat(orig_path, &st);
    TEST_ASSERT(result < 0 && errno == ENOENT,
        "original path should not exist after rename");

    /* Clean up */
    unlink(new_path);

    return 0;
}

/**
 * Test 6: link() triggered copy-up
 *
 * Creating a hard link to a base layer file should trigger copy-up
 * while preserving the original inode number for both paths.
 */
static int test_link_copyup(const char *base_path) {
    char orig_path[512], link_path[512], link2_path[512];
    struct stat st_orig, st_link;
    ino_t orig_ino;
    int result;

    orig_ino = get_base_layer_inode(base_path, "copyup_link_test.txt", "link copyup");
    if (orig_ino == 0) return 0;

    snprintf(orig_path, sizeof(orig_path), "%s/copyup_link_test.txt", base_path);
    snprintf(link_path, sizeof(link_path), "%s/copyup_link_test_hardlink.txt", base_path);
    snprintf(link2_path, sizeof(link2_path), "%s/copyup_link_test_hardlink2.txt", base_path);

    /* Clean up any previous links */
    unlink(link_path);
    unlink(link2_path);

    /* link() - this triggers copy-up */
    result = link(orig_path, link_path);
    if (result < 0 && (errno == ENOSYS || errno == EOPNOTSUPP)) {
        printf("  (Skipping link copyup test - link syscall not supported)\n");
        return 0;
    }
    TEST_ASSERT_ERRNO(result == 0, "link should succeed");

    /* Original file must still have the same inode */
    TEST_ASSERT(check_inode_stable(orig_path, orig_ino, "link (original)") == 0,
        "original inode must remain stable after link copy-up");

    /* Hard link must have the same inode */
    result = stat(link_path, &st_link);
    TEST_ASSERT_ERRNO(result == 0, "stat on hard link should succeed");
    if (st_link.st_ino != orig_ino) {
        fprintf(stderr, "  hard link inode mismatch: expected %lu, got %lu\n",
                (unsigned long)orig_ino, (unsigned long)st_link.st_ino);
    }
    TEST_ASSERT(st_link.st_ino == orig_ino,
        "hard link must have same inode as original");

    /* Verify link count increased */
    result = stat(orig_path, &st_orig);
    TEST_ASSERT_ERRNO(result == 0, "stat on original should succeed");
    TEST_ASSERT(st_orig.st_nlink >= 2,
        "link count should be at least 2 after creating hard link");

    /* Create another hard link and verify */
    result = link(orig_path, link2_path);
    TEST_ASSERT_ERRNO(result == 0, "creating second hard link should succeed");

    result = stat(link2_path, &st_link);
    TEST_ASSERT_ERRNO(result == 0, "stat on second hard link should succeed");
    TEST_ASSERT(st_link.st_ino == orig_ino,
        "second hard link must have same inode as original");

    /* Re-check original still has correct inode */
    TEST_ASSERT(check_inode_stable(orig_path, orig_ino, "link (after second link)") == 0,
        "original inode must remain stable after second link");

    /* lstat should also show consistent inodes */
    result = lstat(orig_path, &st_orig);
    TEST_ASSERT_ERRNO(result == 0, "lstat on original should succeed");
    TEST_ASSERT(st_orig.st_ino == orig_ino,
        "lstat must return same inode after link copy-up");

    result = lstat(link_path, &st_link);
    TEST_ASSERT_ERRNO(result == 0, "lstat on hard link should succeed");
    TEST_ASSERT(st_link.st_ino == orig_ino,
        "lstat on hard link must return same inode");

    /* Unlink one link and verify others still have correct inode */
    result = unlink(link_path);
    TEST_ASSERT_ERRNO(result == 0, "unlink first hard link should succeed");

    TEST_ASSERT(check_inode_stable(orig_path, orig_ino, "link (after unlink)") == 0,
        "original inode must remain stable after unlinking hard link");

    TEST_ASSERT(check_inode_stable(link2_path, orig_ino, "link (remaining link)") == 0,
        "remaining hard link must have same inode after unlink");

    /* Clean up */
    unlink(link2_path);

    return 0;
}

/**
 * Test 7: utimes() / utimensat() triggered copy-up
 *
 * Changing timestamps on a base layer file should trigger copy-up
 * while preserving the original inode number.
 */
static int test_utimes_copyup(const char *base_path) {
    char path[512];
    ino_t orig_ino;
    int result;
    struct timeval times[2];
    struct timespec ts[2];

    orig_ino = get_base_layer_inode(base_path, "copyup_utimes_test.txt", "utimes copyup");
    if (orig_ino == 0) return 0;

    snprintf(path, sizeof(path), "%s/copyup_utimes_test.txt", base_path);

    /* utimes - set both atime and mtime to current time */
    times[0].tv_sec = 1000000000;  /* atime */
    times[0].tv_usec = 0;
    times[1].tv_sec = 1000000000;  /* mtime */
    times[1].tv_usec = 0;

    result = utimes(path, times);
    if (result < 0 && errno == ENOSYS) {
        printf("  (Skipping utimes copyup test - utimes not supported)\n");
        return 0;
    }
    TEST_ASSERT_ERRNO(result == 0, "utimes should succeed");

    /* Verify inode is stable after copy-up */
    TEST_ASSERT(check_inode_stable(path, orig_ino, "utimes") == 0,
        "inode must remain stable after utimes copy-up");

    /* Also test utimensat */
    ts[0].tv_sec = 1000000001;
    ts[0].tv_nsec = 0;
    ts[1].tv_sec = 1000000001;
    ts[1].tv_nsec = 0;

    result = utimensat(AT_FDCWD, path, ts, 0);
    if (result < 0 && errno == ENOSYS) {
        printf("  (utimensat not supported, skipping that part)\n");
        return 0;
    }
    TEST_ASSERT_ERRNO(result == 0, "utimensat should succeed");

    TEST_ASSERT(check_inode_stable(path, orig_ino, "utimensat") == 0,
        "inode must remain stable after utimensat copy-up");

    /* Test futimens via file descriptor */
    int fd = open(path, O_RDWR);
    if (fd >= 0) {
        ts[0].tv_sec = 1000000002;
        ts[1].tv_sec = 1000000002;
        result = futimens(fd, ts);
        if (result == 0) {
            struct stat st;
            result = fstat(fd, &st);
            TEST_ASSERT_ERRNO(result == 0, "fstat after futimens should succeed");
            TEST_ASSERT(st.st_ino == orig_ino,
                "fstat must return stable inode after futimens");
        }
        close(fd);
    }

    return 0;
}

/**
 * Test 8: setxattr() triggered copy-up
 *
 * Setting extended attributes on a base layer file should trigger copy-up
 * while preserving the original inode number.
 *
 * Note: Extended attributes may not be supported on all filesystems.
 */
static int test_xattr_copyup(const char *base_path) {
    char path[512];
    ino_t orig_ino;
    int result;
    const char *value = "test_value";

    orig_ino = get_base_layer_inode(base_path, "copyup_xattr_test.txt", "xattr copyup");
    if (orig_ino == 0) return 0;

    snprintf(path, sizeof(path), "%s/copyup_xattr_test.txt", base_path);

    /* setxattr - this may trigger copy-up */
    result = setxattr(path, "user.test_attr", value, strlen(value), 0);
    if (result < 0) {
        if (errno == ENOTSUP || errno == EOPNOTSUPP || errno == ENOSYS) {
            printf("  (Skipping xattr copyup test - xattr not supported)\n");
            return 0;
        }
        /* Some filesystems return EPERM even though xattr is "supported" */
        if (errno == EPERM) {
            printf("  (Skipping xattr copyup test - permission denied)\n");
            return 0;
        }
    }
    TEST_ASSERT_ERRNO(result == 0, "setxattr should succeed");

    /* Verify inode is stable after copy-up */
    TEST_ASSERT(check_inode_stable(path, orig_ino, "setxattr") == 0,
        "inode must remain stable after setxattr copy-up");

    /* Also test lsetxattr (for non-symlink, should behave same) */
    result = lsetxattr(path, "user.test_attr2", value, strlen(value), 0);
    if (result == 0) {
        TEST_ASSERT(check_inode_stable(path, orig_ino, "lsetxattr") == 0,
            "inode must remain stable after lsetxattr copy-up");
    }

    /* Test removexattr */
    result = removexattr(path, "user.test_attr");
    if (result == 0) {
        TEST_ASSERT(check_inode_stable(path, orig_ino, "removexattr") == 0,
            "inode must remain stable after removexattr copy-up");
    }

    return 0;
}

/**
 * Test 9: fallocate() triggered copy-up
 *
 * Allocating space in a base layer file should trigger copy-up
 * while preserving the original inode number.
 */
static int test_fallocate_copyup(const char *base_path) {
    char path[512];
    ino_t orig_ino;
    int fd, result;

    orig_ino = get_base_layer_inode(base_path, "copyup_fallocate_test.txt", "fallocate copyup");
    if (orig_ino == 0) return 0;

    snprintf(path, sizeof(path), "%s/copyup_fallocate_test.txt", base_path);

    /* Open the file */
    fd = open(path, O_RDWR);
    TEST_ASSERT_ERRNO(fd >= 0, "open for fallocate should succeed");

    /* fallocate - this triggers copy-up */
    result = fallocate(fd, 0, 0, 1024);
    if (result < 0) {
        if (errno == ENOTSUP || errno == EOPNOTSUPP || errno == ENOSYS) {
            printf("  (Skipping fallocate copyup test - fallocate not supported)\n");
            close(fd);
            return 0;
        }
    }
    TEST_ASSERT_ERRNO(result == 0, "fallocate should succeed");

    /* Verify inode is stable via fstat */
    struct stat st;
    result = fstat(fd, &st);
    TEST_ASSERT_ERRNO(result == 0, "fstat after fallocate should succeed");
    if (st.st_ino != orig_ino) {
        fprintf(stderr, "  fstat inode mismatch after fallocate: expected %lu, got %lu\n",
                (unsigned long)orig_ino, (unsigned long)st.st_ino);
    }
    TEST_ASSERT(st.st_ino == orig_ino,
        "fstat must return stable inode after fallocate copy-up");
    close(fd);

    /* Also verify via stat */
    TEST_ASSERT(check_inode_stable(path, orig_ino, "fallocate") == 0,
        "inode must remain stable after fallocate copy-up");

    return 0;
}

/**
 * Main entry point for copyup inode stability tests.
 *
 * Runs all copy-up triggered tests and reports results.
 */
int test_copyup_inode_stability(const char *base_path) {
    int result;

    /* Test 1: write() triggered copy-up */
    result = test_write_copyup(base_path);
    if (result != 0) return result;

    /* Test 2: truncate() triggered copy-up */
    result = test_truncate_copyup(base_path);
    if (result != 0) return result;

    /* Test 3: chmod() triggered copy-up */
    result = test_chmod_copyup(base_path);
    if (result != 0) return result;

    /* Test 4: chown() triggered copy-up */
    result = test_chown_copyup(base_path);
    if (result != 0) return result;

    /* Test 5: rename() triggered copy-up */
    result = test_rename_copyup(base_path);
    if (result != 0) return result;

    /* Test 6: link() triggered copy-up */
    result = test_link_copyup(base_path);
    if (result != 0) return result;

    /* Test 7: utimes() triggered copy-up */
    result = test_utimes_copyup(base_path);
    if (result != 0) return result;

    /* Test 8: setxattr() triggered copy-up */
    result = test_xattr_copyup(base_path);
    if (result != 0) return result;

    /* Test 9: fallocate() triggered copy-up */
    result = test_fallocate_copyup(base_path);
    if (result != 0) return result;

    return 0;
}
