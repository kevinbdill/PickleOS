# Makefile for PICKLE OS user-space programs

CC = gcc
CFLAGS = -std=c11 -Wall -Wextra -O2 -ffreestanding -nostdlib -static -fno-pie -fno-pic
LDFLAGS = -T user.ld -nostdlib -static

PROGRAMS = hello test_lib stdio_test fork_test pipe_test args_test signal_test pid_test calculator net_test pie_test

all: $(PROGRAMS)

hello: hello.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o hello hello.c

test_lib: test_lib.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o test_lib test_lib.c

stdio_test: stdio_test.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o stdio_test stdio_test.c

fork_test: fork_test.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o fork_test fork_test.c

pipe_test: pipe_test.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o pipe_test pipe_test.c

args_test: args_test.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o args_test args_test.c

signal_test: signal_test.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o signal_test signal_test.c

pid_test: pid_test.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o pid_test pid_test.c

calculator: calculator.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o calculator calculator.c

net_test: net_test.c user.ld
	$(CC) $(CFLAGS) $(LDFLAGS) -o net_test net_test.c

pie_test: pie_test.c user_pie.ld
	$(CC) -std=c11 -Wall -Wextra -O2 -ffreestanding -nostdlib -static-pie -fPIE -T user_pie.ld -o pie_test pie_test.c

clean:
	rm -f $(PROGRAMS)

.PHONY: all clean
