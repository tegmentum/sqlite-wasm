/*
 * SQLite WASM In-Memory VFS
 *
 * Loads the whole file into a wasm-linear-memory buffer at xOpen,
 * serves reads/writes from RAM, writes back to the underlying fd on
 * xSync / xClose. The wasi shim cost per page read becomes
 * negligible because we issue O(1) wasi calls per file lifetime
 * (one pread for the whole file, one pwrite per sync) instead of
 * O(pages-touched).
 *
 * Scope (v1):
 *   - main db file + journal + temp files: all buffered
 *   - WAL mode NOT supported (no xShm* methods); users who need
 *     WAL stay on wasivfs by setting SQLITE_WASM_MEMVFS=0 or by
 *     opening with vfs="wasivfs"
 *   - file size effectively capped by available linear memory;
 *     wasmtime reserves 4 GiB virtual on this build so anything
 *     under that fits, but only pages actually touched commit
 *     physical RAM
 *
 * Correctness:
 *   - xSync flushes the whole buffer back to disk; sqlite's commit
 *     path drives this around the standard rollback-journal
 *     protocol so a clean shutdown durably persists.
 *   - Between xSync calls, writes are RAM-only  same window as
 *     synchronous=NORMAL on disk-backed sqlite, just shifted by
 *     the buffer.
 *
 * Performance shape:
 *   - xRead: pure memcpy, no wasi cost
 *   - xWrite: memcpy + dirty flag flip
 *   - xSync: pwrite(fd, buf, length, 0)  one wasi call per sync
 *   - xClose: sync + close
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#include "sqlite3.h"

#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>

#define MEMVFS_MAX_PATHNAME  512
#define MEMVFS_SECTOR_SIZE   4096

typedef struct MemFile MemFile;
struct MemFile {
    sqlite3_file base;
    int fd;                   /* underlying fd kept open for xSync writeback */
    char *path;
    int lock_level;
    int read_only;            /* if SQLITE_OPEN_READONLY, skip xSync writeback */
    int dirty;                /* writes happened since last xSync */
    unsigned char *buf;       /* linear buffer; first `size` bytes are live */
    sqlite3_int64 size;       /* current logical file size in bytes */
    sqlite3_int64 cap;        /* allocated capacity of `buf` */
};

static int g_last_errno = 0;

/* Forward decls */
static int memfile_close(sqlite3_file*);
static int memfile_read(sqlite3_file*, void*, int, sqlite3_int64);
static int memfile_write(sqlite3_file*, const void*, int, sqlite3_int64);
static int memfile_truncate(sqlite3_file*, sqlite3_int64);
static int memfile_sync(sqlite3_file*, int);
static int memfile_filesize(sqlite3_file*, sqlite3_int64*);
static int memfile_lock(sqlite3_file*, int);
static int memfile_unlock(sqlite3_file*, int);
static int memfile_checkreservedlock(sqlite3_file*, int*);
static int memfile_filecontrol(sqlite3_file*, int, void*);
static int memfile_sectorsize(sqlite3_file*);
static int memfile_devicecharacteristics(sqlite3_file*);

static const sqlite3_io_methods g_memfile_methods = {
    1,                          /* iVersion = 1; no xShm* */
    memfile_close,
    memfile_read,
    memfile_write,
    memfile_truncate,
    memfile_sync,
    memfile_filesize,
    memfile_lock,
    memfile_unlock,
    memfile_checkreservedlock,
    memfile_filecontrol,
    memfile_sectorsize,
    memfile_devicecharacteristics,
    /* iVersion=1 stops here */
    NULL, NULL, NULL, NULL,
    NULL, NULL,
};

static sqlite3_vfs g_memvfs;

/* Grow `f->buf` so it can hold at least `need` bytes. Doubles cap;
 * returns SQLITE_OK or SQLITE_IOERR_NOMEM. */
static int memfile_grow(MemFile *f, sqlite3_int64 need) {
    if (need <= f->cap) return SQLITE_OK;
    sqlite3_int64 new_cap = f->cap > 0 ? f->cap : 4096;
    while (new_cap < need) {
        new_cap *= 2;
        if (new_cap < 0) return SQLITE_IOERR_NOMEM;
    }
    unsigned char *new_buf = (unsigned char *)sqlite3_realloc64(f->buf, new_cap);
    if (!new_buf) return SQLITE_IOERR_NOMEM;
    /* Zero the freshly-allocated tail so reads past size return 0
       (sqlite's contract on short reads). */
    if (new_cap > f->cap) {
        memset(new_buf + f->cap, 0, new_cap - f->cap);
    }
    f->buf = new_buf;
    f->cap = new_cap;
    return SQLITE_OK;
}

static int memfile_close(sqlite3_file *pFile) {
    MemFile *f = (MemFile*)pFile;
    if (f->dirty && f->fd >= 0 && !f->read_only) {
        /* Flush before close so a clean shutdown persists. */
        memfile_sync(pFile, 0);
    }
    if (f->fd >= 0) {
        close(f->fd);
        f->fd = -1;
    }
    if (f->buf) {
        sqlite3_free(f->buf);
        f->buf = NULL;
    }
    if (f->path) {
        sqlite3_free(f->path);
        f->path = NULL;
    }
    return SQLITE_OK;
}

static int memfile_read(sqlite3_file *pFile, void *zBuf, int amt, sqlite3_int64 offset) {
    MemFile *f = (MemFile*)pFile;
    if (offset >= f->size) {
        memset(zBuf, 0, amt);
        return SQLITE_IOERR_SHORT_READ;
    }
    sqlite3_int64 available = f->size - offset;
    int to_copy = amt;
    if ((sqlite3_int64)to_copy > available) to_copy = (int)available;
    memcpy(zBuf, f->buf + offset, to_copy);
    if (to_copy < amt) {
        memset((char*)zBuf + to_copy, 0, amt - to_copy);
        return SQLITE_IOERR_SHORT_READ;
    }
    return SQLITE_OK;
}

static int memfile_write(sqlite3_file *pFile, const void *zBuf, int amt, sqlite3_int64 offset) {
    MemFile *f = (MemFile*)pFile;
    if (f->read_only) {
        return SQLITE_IOERR_WRITE;
    }
    sqlite3_int64 need = offset + amt;
    int rc = memfile_grow(f, need);
    if (rc != SQLITE_OK) return rc;
    memcpy(f->buf + offset, zBuf, amt);
    if (need > f->size) f->size = need;
    f->dirty = 1;
    return SQLITE_OK;
}

static int memfile_truncate(sqlite3_file *pFile, sqlite3_int64 size) {
    MemFile *f = (MemFile*)pFile;
    if (f->read_only) return SQLITE_IOERR_TRUNCATE;
    if (size < f->size) {
        /* Zero the tail so a subsequent grow + read returns zeros. */
        memset(f->buf + size, 0, f->size - size);
    }
    f->size = size;
    f->dirty = 1;
    return SQLITE_OK;
}

static int memfile_sync(sqlite3_file *pFile, int flags) {
    MemFile *f = (MemFile*)pFile;
    (void)flags;
    if (!f->dirty || f->fd < 0 || f->read_only) return SQLITE_OK;
    /* Write the whole logical buffer back. Truncate fd to `size`
       so the on-disk file matches. */
    if (f->size > 0) {
        ssize_t w = pwrite(f->fd, f->buf, (size_t)f->size, 0);
        if (w != (ssize_t)f->size) {
            g_last_errno = errno;
            return SQLITE_IOERR_WRITE;
        }
    }
    if (ftruncate(f->fd, (off_t)f->size) != 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_TRUNCATE;
    }
    if (fsync(f->fd) != 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_FSYNC;
    }
    f->dirty = 0;
    return SQLITE_OK;
}

static int memfile_filesize(sqlite3_file *pFile, sqlite3_int64 *pSize) {
    MemFile *f = (MemFile*)pFile;
    *pSize = f->size;
    return SQLITE_OK;
}

static int memfile_lock(sqlite3_file *pFile, int level) {
    MemFile *f = (MemFile*)pFile;
    f->lock_level = level;
    return SQLITE_OK;
}

static int memfile_unlock(sqlite3_file *pFile, int level) {
    MemFile *f = (MemFile*)pFile;
    f->lock_level = level;
    return SQLITE_OK;
}

static int memfile_checkreservedlock(sqlite3_file *pFile, int *pResOut) {
    MemFile *f = (MemFile*)pFile;
    *pResOut = (f->lock_level > SQLITE_LOCK_SHARED) ? 1 : 0;
    return SQLITE_OK;
}

static int memfile_filecontrol(sqlite3_file *pFile, int op, void *pArg) {
    (void)pFile;
    if (op == SQLITE_FCNTL_VFSNAME && pArg != NULL) {
        char **out = (char**)pArg;
        *out = sqlite3_mprintf("%s", "memvfs");
        return SQLITE_OK;
    }
    return SQLITE_NOTFOUND;
}

static int memfile_sectorsize(sqlite3_file *pFile) {
    (void)pFile;
    return MEMVFS_SECTOR_SIZE;
}

static int memfile_devicecharacteristics(sqlite3_file *pFile) {
    (void)pFile;
    /* SAFE_APPEND: writes past EOF don't corrupt anything (we grow
       the buffer). SEQUENTIAL: sqlite can assume that issuing the
       writes in order guarantees they hit disk in order  trivially
       true since we batch the whole buffer on sync. */
    return SQLITE_IOCAP_ATOMIC
         | SQLITE_IOCAP_SAFE_APPEND
         | SQLITE_IOCAP_SEQUENTIAL;
}

static int memvfs_open(sqlite3_vfs *pVfs, const char *zName, sqlite3_file *pFile,
                       int flags, int *pOutFlags) {
    (void)pVfs;
    MemFile *f = (MemFile*)pFile;
    memset(f, 0, sizeof(MemFile));
    f->fd = -1;
    f->lock_level = SQLITE_LOCK_NONE;
    f->read_only = (flags & SQLITE_OPEN_READONLY) ? 1 : 0;

    int oflags = 0;
    if (flags & SQLITE_OPEN_EXCLUSIVE) oflags |= O_EXCL;
    if (flags & SQLITE_OPEN_CREATE)    oflags |= O_CREAT;
    if (flags & SQLITE_OPEN_READONLY)  oflags |= O_RDONLY;
    else if (flags & SQLITE_OPEN_READWRITE) oflags |= O_RDWR;

    /* Temp / anonymous files: synthesize a name like wasivfs does. */
    if (zName == NULL || zName[0] == '\0') {
        static int temp_counter = 0;
        char temp_name[64];
        sqlite3_snprintf(sizeof(temp_name), temp_name, "/tmp/sqlite_memvfs_%d",
                         ++temp_counter);
        zName = temp_name;
        oflags |= O_CREAT | O_RDWR;
        flags &= ~SQLITE_OPEN_READONLY;
        f->read_only = 0;
    }

    f->fd = open(zName, oflags, 0644);
    if (f->fd < 0) {
        g_last_errno = errno;
        return SQLITE_CANTOPEN;
    }

    /* Read whole file into buffer. fstat for size first; pread once. */
    struct stat st;
    if (fstat(f->fd, &st) != 0) {
        g_last_errno = errno;
        close(f->fd);
        f->fd = -1;
        return SQLITE_IOERR_FSTAT;
    }
    f->size = (sqlite3_int64)st.st_size;
    if (f->size > 0) {
        int rc = memfile_grow(f, f->size);
        if (rc != SQLITE_OK) {
            close(f->fd);
            f->fd = -1;
            return rc;
        }
        ssize_t got = pread(f->fd, f->buf, (size_t)f->size, 0);
        if (got != (ssize_t)f->size) {
            g_last_errno = errno;
            close(f->fd);
            sqlite3_free(f->buf);
            f->fd = -1;
            f->buf = NULL;
            return SQLITE_IOERR_READ;
        }
    }

    f->path = sqlite3_mprintf("%s", zName);
    f->base.pMethods = &g_memfile_methods;
    if (pOutFlags) *pOutFlags = flags;
    return SQLITE_OK;
}

static int memvfs_delete(sqlite3_vfs *pVfs, const char *zName, int syncDir) {
    (void)pVfs;
    (void)syncDir;
    if (unlink(zName) != 0) {
        g_last_errno = errno;
        if (errno == ENOENT) return SQLITE_OK;
        return SQLITE_IOERR_DELETE;
    }
    return SQLITE_OK;
}

static int memvfs_access(sqlite3_vfs *pVfs, const char *zName, int flags, int *pResOut) {
    (void)pVfs;
    int mode;
    switch (flags) {
        case SQLITE_ACCESS_EXISTS:    mode = F_OK; break;
        case SQLITE_ACCESS_READ:      mode = R_OK; break;
        case SQLITE_ACCESS_READWRITE: mode = R_OK | W_OK; break;
        default: *pResOut = 0; return SQLITE_OK;
    }
    *pResOut = (access(zName, mode) == 0) ? 1 : 0;
    return SQLITE_OK;
}

static int memvfs_fullpathname(sqlite3_vfs *pVfs, const char *zName, int nOut, char *zOut) {
    (void)pVfs;
    if (zName == NULL) { zOut[0] = '\0'; return SQLITE_OK; }
    int len = (int)strlen(zName);
    if (len >= nOut) return SQLITE_CANTOPEN;
    memcpy(zOut, zName, len + 1);
    return SQLITE_OK;
}

static void *memvfs_dlopen(sqlite3_vfs *pVfs, const char *zFilename) {
    (void)pVfs; (void)zFilename; return NULL;
}
static void memvfs_dlerror(sqlite3_vfs *pVfs, int nByte, char *zErrMsg) {
    (void)pVfs;
    if (nByte > 0) sqlite3_snprintf(nByte, zErrMsg, "Dynamic loading not supported");
}
static void (*memvfs_dlsym(sqlite3_vfs *pVfs, void *p, const char *z))(void) {
    (void)pVfs; (void)p; (void)z; return NULL;
}
static void memvfs_dlclose(sqlite3_vfs *pVfs, void *p) { (void)pVfs; (void)p; }

static int memvfs_randomness(sqlite3_vfs *pVfs, int nByte, char *zOut) {
    (void)pVfs;
    int fd = open("/dev/urandom", O_RDONLY);
    if (fd >= 0) {
        ssize_t got = read(fd, zOut, nByte);
        close(fd);
        if (got == nByte) return nByte;
    }
    static uint32_t seed = 0;
    if (seed == 0) seed = (uint32_t)time(NULL);
    for (int i = 0; i < nByte; i++) {
        seed = seed * 1103515245 + 12345;
        zOut[i] = (char)(seed >> 16);
    }
    return nByte;
}

static int memvfs_sleep(sqlite3_vfs *pVfs, int microseconds) {
    (void)pVfs;
    volatile int i;
    for (i = 0; i < microseconds / 10; i++) {}
    return microseconds;
}

static int memvfs_currenttime(sqlite3_vfs *pVfs, double *pTime) {
    (void)pVfs;
    time_t t = time(NULL);
    *pTime = (double)t / 86400.0 + 2440587.5;
    return SQLITE_OK;
}

static int memvfs_getlasterror(sqlite3_vfs *pVfs, int nBuf, char *zBuf) {
    (void)pVfs;
    if (nBuf > 0 && zBuf) sqlite3_snprintf(nBuf, zBuf, "errno=%d", g_last_errno);
    return g_last_errno;
}

static int memvfs_currenttimeint64(sqlite3_vfs *pVfs, sqlite3_int64 *pNow) {
    (void)pVfs;
    time_t t = time(NULL);
    /* Julian-day milliseconds, matching wasivfs's shape. */
    *pNow = ((sqlite3_int64)t * 1000) + 210866760000000LL;
    return SQLITE_OK;
}

/* Public registration. makeDefault=1 routes every open through us. */
int sqlite3_memvfs_register(int makeDefault) {
    static int initialized = 0;
    if (initialized) return SQLITE_OK;

    memset(&g_memvfs, 0, sizeof(g_memvfs));
    g_memvfs.iVersion = 2;
    g_memvfs.szOsFile = sizeof(MemFile);
    g_memvfs.mxPathname = MEMVFS_MAX_PATHNAME;
    g_memvfs.pNext = NULL;
    g_memvfs.zName = "memvfs";
    g_memvfs.pAppData = NULL;
    g_memvfs.xOpen = memvfs_open;
    g_memvfs.xDelete = memvfs_delete;
    g_memvfs.xAccess = memvfs_access;
    g_memvfs.xFullPathname = memvfs_fullpathname;
    g_memvfs.xDlOpen = memvfs_dlopen;
    g_memvfs.xDlError = memvfs_dlerror;
    g_memvfs.xDlSym = memvfs_dlsym;
    g_memvfs.xDlClose = memvfs_dlclose;
    g_memvfs.xRandomness = memvfs_randomness;
    g_memvfs.xSleep = memvfs_sleep;
    g_memvfs.xCurrentTime = memvfs_currenttime;
    g_memvfs.xGetLastError = memvfs_getlasterror;
    g_memvfs.xCurrentTimeInt64 = memvfs_currenttimeint64;

    int rc = sqlite3_vfs_register(&g_memvfs, makeDefault);
    if (rc == SQLITE_OK) initialized = 1;
    return rc;
}

const char *sqlite3_memvfs_name(void) { return "memvfs"; }
