CC = gcc
CFLAGS = -O2 -Wall -Wextra

TARGETS = perf-statx perf-open-close

all: $(TARGETS)

%: %.c
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f $(TARGETS)

.PHONY: all clean
