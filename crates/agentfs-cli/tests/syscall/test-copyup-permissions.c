#define _GNU_SOURCE
#include "test-common.h"
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>

/**
 * Test that copy-up preserves base layer file permissions.
 *
 * When a file is copied from the base layer to the delta layer (copy-up),
 * its permissions must be preserved. This test uses a file that was created
 * in the base layer (by the test shell script) with mode 0755.
 *
 * The bug: copy-up code was using DEFAULT_FILE_MODE (0644) instead of
 * preserving the original stats.mode from the base layer.
 *
 * Test setup (in test-run-syscalls.sh):
 *   echo -n "executable content" > executable_base.txt
 *   chmod 0755 executable_base.txt
 *
 * The test:
 * 1. Verifies the base layer file has mode 0755
 * 2. Writes to the file (triggers copy-up to delta layer)
 * 3. Verifies that permissions are still 0755 after copy-up
 */

int test_copyup_permissions(const char *base_path) {
    char file_path[512];
    struct stat st_before, st_after;
    int result, fd;
    mode_t mode_before, mode_after;

    snprintf(file_path, sizeof(file_path), "%s/executable_base.txt", base_path);

    /*
     * Test 1: Verify the base layer file exists and has mode 0755.
     * This file was created by the test shell script before mounting the overlay.
     */
    result = stat(file_path, &st_before);
    if (result < 0 && errno == ENOENT) {
        printf("  (Skipping copy-up permissions test - executable_base.txt not in base layer)\n");
        return 0;
    }
    TEST_ASSERT_ERRNO(result == 0, "stat on base layer file should succeed");

    mode_before = st_before.st_mode & 07777;  /* Extract permission bits only */
    if (mode_before != 0755) {
        fprintf(stderr, "  base layer mode: expected 0755, got %04o\n", mode_before);
        fprintf(stderr, "  (test requires executable_base.txt with mode 0755 in base layer)\n");
    }
    TEST_ASSERT(mode_before == 0755, "base layer file should have mode 0755");

    /*
     * Test 2: Write to the file to trigger copy-up.
     * In the overlay filesystem, this causes the file to be copied from
     * the base layer to the delta layer. The bug was that copy-up used
     * DEFAULT_FILE_MODE (0644) instead of preserving the original mode.
     */
    fd = open(file_path, O_WRONLY | O_APPEND);
    TEST_ASSERT_ERRNO(fd >= 0, "open base layer file for append should succeed");
    result = write(fd, " appended", 9);
    TEST_ASSERT_ERRNO(result == 9, "append to base layer file should succeed");
    close(fd);

    /*
     * Test 3: THE CRITICAL TEST - verify permissions after copy-up.
     * The mode must be 0755 (same as the base layer file), NOT 0644.
     */
    result = stat(file_path, &st_after);
    TEST_ASSERT_ERRNO(result == 0, "stat after copy-up should succeed");

    mode_after = st_after.st_mode & 07777;
    if (mode_after != mode_before) {
        fprintf(stderr, "  COPY-UP PERMISSION BUG: mode changed from %04o to %04o\n",
                mode_before, mode_after);
        fprintf(stderr, "  Expected: %04o (preserved from base layer)\n", mode_before);
        fprintf(stderr, "  Got:      %04o (likely DEFAULT_FILE_MODE)\n", mode_after);
    }
    TEST_ASSERT(mode_after == mode_before,
        "file permissions must be preserved after copy-up (was 0755, should still be 0755)");

    /*
     * Test 4: Also verify via fstat on open file descriptor.
     */
    fd = open(file_path, O_RDONLY);
    TEST_ASSERT_ERRNO(fd >= 0, "open copied-up file should succeed");

    result = fstat(fd, &st_after);
    TEST_ASSERT_ERRNO(result == 0, "fstat on copied-up file should succeed");

    mode_after = st_after.st_mode & 07777;
    TEST_ASSERT(mode_after == mode_before,
        "fstat must also show preserved permissions after copy-up");
    close(fd);

    return 0;
}
