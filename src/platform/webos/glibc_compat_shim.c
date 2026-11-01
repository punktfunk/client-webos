/* webOS's shipped glibc (~2.12, confirmed via the sysroot's libc.so.6 GLIBC_*
 * version-string set) predates several glibc functions Rust std/punktfunk-core link
 * against unconditionally: getauxval (glibc 2.16), gettid (glibc 2.30), and sendmmsg
 * (glibc 2.14). Each is reimplemented here as a thin wrapper — getauxval via
 * /proc/self/auxv (no raw syscall for it), gettid/sendmmsg via direct syscall(2),
 * which itself predates all of these by a wide margin. See build.rs for when this
 * gets compiled in. */
#include <stdio.h>
#include <unistd.h>
#include <sys/syscall.h>

unsigned long getauxval(unsigned long type) {
    struct {
        unsigned long a_type;
        unsigned long a_val;
    } aux;
    unsigned long result = 0;

    FILE *f = fopen("/proc/self/auxv", "rb");
    if (!f) {
        return 0;
    }
    while (fread(&aux, sizeof(aux), 1, f) == 1) {
        if (aux.a_type == 0) {
            break; /* AT_NULL terminator */
        }
        if (aux.a_type == type) {
            result = aux.a_val;
            break;
        }
    }
    fclose(f);
    return result;
}

/* Kernel thread id — SYS_gettid is defined in this sysroot's own asm/unistd.h
 * (confirmed: __NR_gettid = 224 on ARM EABI) even though glibc doesn't wrap it yet. */
int gettid(void) {
    return (int) syscall(SYS_gettid);
}

/* Batched sendmsg. We deliberately don't declare the real `struct mmsghdr *` type
 * here (this old sysroot's headers may not define it) — syscall(2) forwards the
 * pointer to the kernel untouched, so the exact C type is irrelevant to correctness;
 * only the calling convention (register order or that this shim never dereferences
 * it) needs to match, and it does. */
int sendmmsg(int sockfd, void *msgvec, unsigned int vlen, int flags) {
    return (int) syscall(SYS_sendmmsg, sockfd, msgvec, vlen, flags);
}

int posix_memalign(void **memptr, size_t alignment, size_t size);

void *aligned_alloc(size_t alignment, size_t size) {
    void *p = NULL;
    if (alignment < sizeof(void *))
        alignment = sizeof(void *);
    return posix_memalign(&p, alignment, size) == 0 ? p : NULL;
}
