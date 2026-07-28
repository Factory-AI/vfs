#define _GNU_SOURCE
#include "test-common.h"
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>

/*
 * Tests for O_APPEND on EXISTING files.
 *
 * This is critical for overlay filesystems where the file exists in the
 * base layer and must be copied-on-write when modified.
 *
 * The test harness MUST create "existing.txt" with known content BEFORE
 * running this test.
 */

int test_append_existing(const char *base_path) {
    char path[512];
    char read_buf[1024];
    int fd;
    ssize_t n;
    struct stat st;

    snprintf(path, sizeof(path), "%s/existing.txt", base_path);

    /* Verify the file exists (created by test harness) */
    if (stat(path, &st) != 0) {
        fprintf(stderr, "  Note: existing.txt not found, skipping test_append_existing\n");
        fprintf(stderr, "  (This test requires the harness to create existing.txt first)\n");
        return 0;  /* Skip, not fail */
    }

    off_t original_size = st.st_size;
    printf("  existing.txt found, size=%ld bytes\n", (long)original_size);

    /* Read original content */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open existing.txt for read should succeed");

    n = read(fd, read_buf, sizeof(read_buf) - 1);
    TEST_ASSERT_ERRNO(n >= 0, "read original content should succeed");
    read_buf[n] = '\0';
    close(fd);

    char original_content[1024];
    strncpy(original_content, read_buf, sizeof(original_content) - 1);
    original_content[sizeof(original_content) - 1] = '\0';
    printf("  original content: \"%s\"\n", original_content);

    /* Test 1: Open with O_APPEND and write */
    fd = open(path, O_WRONLY | O_APPEND);
    TEST_ASSERT_ERRNO(fd >= 0, "open with O_APPEND should succeed");

    const char *append_data = "[APPENDED]";
    n = write(fd, append_data, strlen(append_data));
    TEST_ASSERT_ERRNO(n == (ssize_t)strlen(append_data), "append write should write all bytes");
    close(fd);

    /* Test 2: Verify file size increased */
    TEST_ASSERT_ERRNO(stat(path, &st) == 0, "stat after append should succeed");
    TEST_ASSERT(st.st_size == original_size + (off_t)strlen(append_data),
                "file size should increase by appended bytes");
    printf("  after append, size=%ld bytes\n", (long)st.st_size);

    /* Test 3: Read back and verify content */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open for read after append should succeed");

    n = read(fd, read_buf, sizeof(read_buf) - 1);
    TEST_ASSERT_ERRNO(n > 0, "read after append should succeed");
    read_buf[n] = '\0';
    close(fd);

    printf("  after append content: \"%s\"\n", read_buf);

    /* Verify original content is preserved at the start */
    TEST_ASSERT(strncmp(read_buf, original_content, strlen(original_content)) == 0,
                "original content should be preserved");

    /* Verify appended content is at the end */
    TEST_ASSERT(strcmp(read_buf + strlen(original_content), append_data) == 0,
                "appended content should be at the end");

    /* Test 4: Multiple appends */
    fd = open(path, O_WRONLY | O_APPEND);
    TEST_ASSERT_ERRNO(fd >= 0, "second open with O_APPEND should succeed");

    const char *append_data2 = "[MORE]";
    n = write(fd, append_data2, strlen(append_data2));
    TEST_ASSERT_ERRNO(n == (ssize_t)strlen(append_data2), "second append should succeed");
    close(fd);

    /* Verify both appends are present */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open for final read should succeed");

    n = read(fd, read_buf, sizeof(read_buf) - 1);
    TEST_ASSERT_ERRNO(n > 0, "final read should succeed");
    read_buf[n] = '\0';
    close(fd);

    printf("  final content: \"%s\"\n", read_buf);

    /* Build expected content */
    char expected[1024];
    snprintf(expected, sizeof(expected), "%s%s%s", original_content, append_data, append_data2);

    TEST_ASSERT(strcmp(read_buf, expected) == 0,
                "final content should match original + both appends");

    /* Test 5: O_APPEND with O_RDWR */
    fd = open(path, O_RDWR | O_APPEND);
    TEST_ASSERT_ERRNO(fd >= 0, "open with O_RDWR | O_APPEND should succeed");

    /* Read should work from beginning */
    n = read(fd, read_buf, 5);
    TEST_ASSERT_ERRNO(n > 0, "read with O_RDWR | O_APPEND should succeed");

    /* Write should still append */
    const char *append_data3 = "[END]";
    n = write(fd, append_data3, strlen(append_data3));
    TEST_ASSERT_ERRNO(n == (ssize_t)strlen(append_data3), "write with O_RDWR | O_APPEND should append");
    close(fd);

    /* Final verification */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "final open should succeed");
    n = read(fd, read_buf, sizeof(read_buf) - 1);
    read_buf[n] = '\0';
    close(fd);

    snprintf(expected, sizeof(expected), "%s%s%s%s", original_content, append_data, append_data2, append_data3);
    TEST_ASSERT(strcmp(read_buf, expected) == 0,
                "content after O_RDWR | O_APPEND should be correct");

    printf("  all append tests passed\n");

    return 0;
}

/*
 * Test for pwrite on files in NESTED directories (COW parent dir bug).
 *
 * This catches the bug where copy-on-write fails because parent directories
 * don't exist in the delta layer. This is exactly what happens with git:
 * .git/logs/HEAD exists in base layer, but .git/logs/ doesn't exist in delta.
 *
 * The bug is NOT specific to O_APPEND - any pwrite to a nested base-layer file
 * would fail. We use O_APPEND here just as a convenient way to trigger pwrite.
 *
 * The test harness MUST create "subdir/nested.txt" BEFORE running this test.
 */
int test_pwrite_nested(const char *base_path) {
    char path[512];
    char read_buf[1024];
    int fd;
    ssize_t n;
    struct stat st;

    snprintf(path, sizeof(path), "%s/subdir/nested.txt", base_path);

    /* Verify the file exists (created by test harness) */
    if (stat(path, &st) != 0) {
        fprintf(stderr, "  Note: subdir/nested.txt not found, skipping test_append_nested\n");
        fprintf(stderr, "  (This test requires the harness to create subdir/nested.txt first)\n");
        return 0;  /* Skip, not fail */
    }

    off_t original_size = st.st_size;
    printf("  subdir/nested.txt found, size=%ld bytes\n", (long)original_size);

    /* Read original content */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open nested file for read should succeed");

    n = read(fd, read_buf, sizeof(read_buf) - 1);
    TEST_ASSERT_ERRNO(n >= 0, "read original content should succeed");
    read_buf[n] = '\0';
    close(fd);

    char original_content[1024];
    strncpy(original_content, read_buf, sizeof(original_content) - 1);
    original_content[sizeof(original_content) - 1] = '\0';
    printf("  original content: \"%s\"\n", original_content);

    /* Test: Open with O_APPEND and write - this triggers COW */
    /* The bug was that parent dirs weren't created in delta, causing EIO */
    fd = open(path, O_WRONLY | O_APPEND);
    TEST_ASSERT_ERRNO(fd >= 0, "open nested file with O_APPEND should succeed");

    const char *append_data = "[NESTED_APPEND]";
    n = write(fd, append_data, strlen(append_data));
    TEST_ASSERT_ERRNO(n == (ssize_t)strlen(append_data),
                      "append to nested file should succeed (tests COW with parent dirs)");
    close(fd);

    /* Verify file size increased */
    TEST_ASSERT_ERRNO(stat(path, &st) == 0, "stat after append should succeed");
    TEST_ASSERT(st.st_size == original_size + (off_t)strlen(append_data),
                "nested file size should increase by appended bytes");
    printf("  after append, size=%ld bytes\n", (long)st.st_size);

    /* Read back and verify content */
    fd = open(path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open nested file for read after append should succeed");

    n = read(fd, read_buf, sizeof(read_buf) - 1);
    TEST_ASSERT_ERRNO(n > 0, "read after append should succeed");
    read_buf[n] = '\0';
    close(fd);

    printf("  after append content: \"%s\"\n", read_buf);

    /* Verify content is correct */
    char expected[1024];
    snprintf(expected, sizeof(expected), "%s%s", original_content, append_data);
    TEST_ASSERT(strcmp(read_buf, expected) == 0,
                "nested file content should match original + append");

    printf("  nested pwrite test passed (COW with parent dirs works)\n");

    return 0;
}
