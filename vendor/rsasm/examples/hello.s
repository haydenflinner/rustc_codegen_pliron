/* The example from the README: a freestanding Linux x86-64 program.
 *
 * Assemble, link and run it with:
 *
 *     rsasm -o hello.o examples/hello.s && ld -o hello hello.o && ./hello
 *
 * CI does exactly that, which is what proves the object files rsasm writes
 * are real rather than merely well-formed byte for byte.
 */

        .section .rodata
msg:    .ascii  "Hello from rsasm!\n"
msglen = . - msg

        .text
        .globl  _start
        .type   _start, @function
_start:
        movq    $1, %rax                # write
        movq    $1, %rdi                # stdout
        leaq    msg(%rip), %rsi
        movq    $msglen, %rdx
        syscall

        movq    $60, %rax               # exit
        xorq    %rdi, %rdi
        syscall
        .size   _start, . - _start
