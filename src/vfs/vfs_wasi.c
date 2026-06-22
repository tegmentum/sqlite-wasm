/*
 * SQLite WASM WASI VFS Implementation
 *
 * Provides a virtual file system backed by WASI filesystem APIs.
 * This allows SQLite to persist data to the host filesystem when running
 * in WASI-compatible runtimes like wasmtime.
 *
 * Note: WASI does not support file locking, so this VFS operates in
 * a serialized mode suitable for single-process access.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#include "sqlite3.h"

/* WASI-specific includes - available in wasi-sdk */
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>

/* Configuration */
#define WASIVFS_MAX_PATHNAME  512
#define WASIVFS_SECTOR_SIZE   4096

/* Forward declarations */
typedef struct WasiFile WasiFile;

/* SHM-related state attached to the main db file. The wal-index
 * is held in process memory (we don't write the .db-shm file).
 * SQLite rebuilds the index from .db-wal frames on open  the
 * disk wal-index is just a coordination cache between processes,
 * which we don't need for a single-process cli.
 */
#define WASIVFS_SHM_NLOCK 8
typedef struct WasiShm WasiShm;
struct WasiShm {
    int refcount;                       /* open WasiFile handles */
    int n_regions;                       /* allocated regions */
    void **regions;                      /* malloc'd region pages */
    int region_size;                     /* bytes per region (set on first xShmMap) */
    int locks[WASIVFS_SHM_NLOCK];       /* 0 unlocked; >0 shared holders; -1 exclusive */
};

/* WASI file structure */
struct WasiFile {
    sqlite3_file base;      /* Base class - must be first */
    int fd;                 /* File descriptor */
    char *path;             /* File path for debugging */
    int lock_level;         /* Current lock level (simulated) */
    WasiShm *shm;           /* lazy-alloc'd on first xShmMap; main db files only */
};

/* Forward declarations of VFS methods */
static int wasivfs_open(sqlite3_vfs*, const char*, sqlite3_file*, int, int*);
static int wasivfs_delete(sqlite3_vfs*, const char*, int);
static int wasivfs_access(sqlite3_vfs*, const char*, int, int*);
static int wasivfs_fullpathname(sqlite3_vfs*, const char*, int, char*);
static void *wasivfs_dlopen(sqlite3_vfs*, const char*);
static void wasivfs_dlerror(sqlite3_vfs*, int, char*);
static void (*wasivfs_dlsym(sqlite3_vfs*, void*, const char*))(void);
static void wasivfs_dlclose(sqlite3_vfs*, void*);
static int wasivfs_randomness(sqlite3_vfs*, int, char*);
static int wasivfs_sleep(sqlite3_vfs*, int);
static int wasivfs_currenttime(sqlite3_vfs*, double*);
static int wasivfs_getlasterror(sqlite3_vfs*, int, char*);
static int wasivfs_currenttimeint64(sqlite3_vfs*, sqlite3_int64*);

/* Forward declarations of file methods */
static int wasifile_close(sqlite3_file*);
static int wasifile_read(sqlite3_file*, void*, int, sqlite3_int64);
static int wasifile_write(sqlite3_file*, const void*, int, sqlite3_int64);
static int wasifile_truncate(sqlite3_file*, sqlite3_int64);
static int wasifile_sync(sqlite3_file*, int);
static int wasifile_filesize(sqlite3_file*, sqlite3_int64*);
static int wasifile_lock(sqlite3_file*, int);
static int wasifile_unlock(sqlite3_file*, int);
static int wasifile_checkreservedlock(sqlite3_file*, int*);
static int wasifile_filecontrol(sqlite3_file*, int, void*);
static int wasifile_sectorsize(sqlite3_file*);
static int wasifile_devicecharacteristics(sqlite3_file*);
static int wasifile_shmmap(sqlite3_file*, int iPg, int pgsz, int isWrite, void volatile **pp);
static int wasifile_shmlock(sqlite3_file*, int offset, int n, int flags);
static void wasifile_shmbarrier(sqlite3_file*);
static int wasifile_shmunmap(sqlite3_file*, int deleteFlag);

/* I/O methods structure */
static const sqlite3_io_methods g_wasifile_methods = {
    2,                          /* iVersion (2 = Shm* methods present) */
    wasifile_close,
    wasifile_read,
    wasifile_write,
    wasifile_truncate,
    wasifile_sync,
    wasifile_filesize,
    wasifile_lock,
    wasifile_unlock,
    wasifile_checkreservedlock,
    wasifile_filecontrol,
    wasifile_sectorsize,
    wasifile_devicecharacteristics,
    /* Version 2+ methods  in-memory shm for single-process WAL */
    wasifile_shmmap,
    wasifile_shmlock,
    wasifile_shmbarrier,
    wasifile_shmunmap,
    /* Version 3+ methods */
    NULL,                       /* xFetch */
    NULL                        /* xUnfetch */
};

/* Global VFS instance */
static sqlite3_vfs g_wasivfs;
static int g_last_errno = 0;

/*
 * File method implementations
 */

static int wasifile_close(sqlite3_file *pFile) {
    WasiFile *file = (WasiFile*)pFile;

    if (file->fd >= 0) {
        close(file->fd);
        file->fd = -1;
    }

    if (file->path) {
        sqlite3_free(file->path);
        file->path = NULL;
    }

    return SQLITE_OK;
}

static int wasifile_read(sqlite3_file *pFile, void *buf, int amt, sqlite3_int64 offset) {
    WasiFile *file = (WasiFile*)pFile;

    /* pread combines seek + read in a single wasi call (fd_pread)
       instead of fd_seek + fd_read. SQLite reads pages at known
       offsets, so positional I/O is the right primitive. The
       offset->kernel cursor side-effect of lseek+read also means
       the file cursor stays at 0; sqlite never reads sequentially
       through the cursor, so nothing to preserve. Saves one wasi
       roundtrip per page read. */
    ssize_t got = pread(file->fd, buf, amt, (off_t)offset);
    if (got < 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_READ;
    }

    if (got < amt) {
        /* Zero-fill remainder */
        memset((char*)buf + got, 0, amt - got);
        return SQLITE_IOERR_SHORT_READ;
    }

    return SQLITE_OK;
}

static int wasifile_write(sqlite3_file *pFile, const void *buf, int amt, sqlite3_int64 offset) {
    WasiFile *file = (WasiFile*)pFile;

    /* pwrite, same idea as pread above: single wasi call instead
       of fd_seek + fd_write per page. */
    ssize_t written = pwrite(file->fd, buf, amt, (off_t)offset);
    if (written != amt) {
        g_last_errno = errno;
        return SQLITE_IOERR_WRITE;
    }

    return SQLITE_OK;
}

static int wasifile_truncate(sqlite3_file *pFile, sqlite3_int64 size) {
    WasiFile *file = (WasiFile*)pFile;

    if (ftruncate(file->fd, (off_t)size) != 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_TRUNCATE;
    }

    return SQLITE_OK;
}

static int wasifile_sync(sqlite3_file *pFile, int flags) {
    WasiFile *file = (WasiFile*)pFile;
    (void)flags;

    if (fsync(file->fd) != 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_FSYNC;
    }

    return SQLITE_OK;
}

static int wasifile_filesize(sqlite3_file *pFile, sqlite3_int64 *pSize) {
    WasiFile *file = (WasiFile*)pFile;
    struct stat st;

    if (fstat(file->fd, &st) != 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_FSTAT;
    }

    *pSize = (sqlite3_int64)st.st_size;
    return SQLITE_OK;
}

static int wasifile_lock(sqlite3_file *pFile, int level) {
    WasiFile *file = (WasiFile*)pFile;
    /*
     * WASI does not support file locking.
     * We simulate locks for single-process use.
     * Multi-process scenarios are not supported.
     */
    file->lock_level = level;
    return SQLITE_OK;
}

static int wasifile_unlock(sqlite3_file *pFile, int level) {
    WasiFile *file = (WasiFile*)pFile;
    file->lock_level = level;
    return SQLITE_OK;
}

static int wasifile_checkreservedlock(sqlite3_file *pFile, int *pResOut) {
    WasiFile *file = (WasiFile*)pFile;
    /* No other process can hold a lock in WASI */
    *pResOut = (file->lock_level > SQLITE_LOCK_SHARED) ? 1 : 0;
    return SQLITE_OK;
}

static int wasifile_filecontrol(sqlite3_file *pFile, int op, void *pArg) {
    (void)pFile;
    if (op == SQLITE_FCNTL_VFSNAME && pArg != NULL) {
        // sqlite3_mprintf the vfs name into *pArg per the
        // SQLITE_FCNTL_VFSNAME contract. The caller is responsible
        // for sqlite3_free'ing it.
        char **out = (char**)pArg;
        *out = sqlite3_mprintf("%s", "wasivfs");
        return SQLITE_OK;
    }
    return SQLITE_NOTFOUND;
}

static int wasifile_sectorsize(sqlite3_file *pFile) {
    (void)pFile;
    return WASIVFS_SECTOR_SIZE;
}

static int wasifile_devicecharacteristics(sqlite3_file *pFile) {
    (void)pFile;
    return SQLITE_IOCAP_UNDELETABLE_WHEN_OPEN;
}

/*
 * Shared-memory (xShm*) for WAL  in-memory only.
 *
 * SQLite's WAL mode coordinates writers + readers via a small index
 * file (.db-shm) that lives in shared memory. For multi-process
 * access the index must be mmap'd from a real on-disk file; for a
 * single-process cli like ours, an in-process buffer is enough: the
 * index is just a cached view of the .db-wal frames, which SQLite
 * rebuilds at open time. We don't write the .db-shm file at all.
 *
 * Locks become trivial bookkeeping because we're single-threaded
 * single-connection. If we ever go multi-connection, the SHM region
 * + lock state would need to be hoisted into a global registry
 * keyed by the on-disk db path.
 *
 * NOTE: Until SQLite 3.46  3.53 (libsqlite3-sys 0.30  0.38)
 * upstream sqlite3.c defined `SQLITE_OMIT_WAL` when `__wasi__`
 * was set, with the comment "because it requires shared memory
 * APIs". 3.53.2 dropped that defeat  the `__wasi__` block now
 * only OMITs LOAD_EXTENSION + zeroes THREADSAFE. PRAGMA
 * journal_mode=WAL now reaches the shm hooks below.
 */

static int wasifile_shmmap(sqlite3_file *pFile, int iPg, int pgsz,
                            int isWrite, void volatile **pp) {
    WasiFile *file = (WasiFile*)pFile;
    if (!file->shm) {
        file->shm = (WasiShm*)sqlite3_malloc(sizeof(WasiShm));
        if (!file->shm) return SQLITE_NOMEM;
        memset(file->shm, 0, sizeof(WasiShm));
        file->shm->refcount = 1;
        file->shm->region_size = pgsz;
    }
    WasiShm *shm = file->shm;
    /* Page index out of range  grow if isWrite, else return NULL. */
    if (iPg >= shm->n_regions) {
        if (!isWrite) {
            *pp = 0;
            return SQLITE_OK;
        }
        int new_n = iPg + 1;
        void **new_regions = (void**)sqlite3_realloc(shm->regions,
                                                     new_n * sizeof(void*));
        if (!new_regions) return SQLITE_NOMEM;
        for (int i = shm->n_regions; i < new_n; i++) new_regions[i] = NULL;
        shm->regions = new_regions;
        shm->n_regions = new_n;
    }
    if (!shm->regions[iPg]) {
        if (!isWrite) {
            *pp = 0;
            return SQLITE_OK;
        }
        shm->regions[iPg] = sqlite3_malloc(pgsz);
        if (!shm->regions[iPg]) return SQLITE_NOMEM;
        memset(shm->regions[iPg], 0, pgsz);
    }
    *pp = shm->regions[iPg];
    return SQLITE_OK;
}

static int wasifile_shmlock(sqlite3_file *pFile, int offset, int n, int flags) {
    WasiFile *file = (WasiFile*)pFile;
    if (!file->shm) return SQLITE_OK;
    WasiShm *shm = file->shm;
    if (offset < 0 || offset + n > WASIVFS_SHM_NLOCK) return SQLITE_RANGE;
    /* In single-process single-threaded mode, lock contention is
     * impossible. We still maintain the lock state so SQLite's
     * internal sanity checks see consistent values. */
    if (flags & SQLITE_SHM_UNLOCK) {
        for (int i = offset; i < offset + n; i++) {
            if (shm->locks[i] > 0) shm->locks[i]--;
            else if (shm->locks[i] == -1) shm->locks[i] = 0;
        }
    } else if (flags & SQLITE_SHM_EXCLUSIVE) {
        for (int i = offset; i < offset + n; i++) {
            if (shm->locks[i] != 0) return SQLITE_BUSY;
        }
        for (int i = offset; i < offset + n; i++) shm->locks[i] = -1;
    } else { /* SQLITE_SHM_SHARED */
        for (int i = offset; i < offset + n; i++) {
            if (shm->locks[i] == -1) return SQLITE_BUSY;
        }
        for (int i = offset; i < offset + n; i++) shm->locks[i]++;
    }
    return SQLITE_OK;
}

static void wasifile_shmbarrier(sqlite3_file *pFile) {
    (void)pFile;
    /* Single-threaded WASI  no actual memory barrier needed. */
}

static int wasifile_shmunmap(sqlite3_file *pFile, int deleteFlag) {
    WasiFile *file = (WasiFile*)pFile;
    (void)deleteFlag;  /* we never wrote the .db-shm file */
    if (!file->shm) return SQLITE_OK;
    WasiShm *shm = file->shm;
    shm->refcount--;
    if (shm->refcount <= 0) {
        for (int i = 0; i < shm->n_regions; i++) {
            if (shm->regions[i]) sqlite3_free(shm->regions[i]);
        }
        if (shm->regions) sqlite3_free(shm->regions);
        sqlite3_free(shm);
    }
    file->shm = NULL;
    return SQLITE_OK;
}

/*
 * VFS method implementations
 */

static int wasivfs_open(sqlite3_vfs *pVfs, const char *zName, sqlite3_file *pFile,
                        int flags, int *pOutFlags) {
    (void)pVfs;
    WasiFile *file = (WasiFile*)pFile;
    int oflags = 0;
    int rc = SQLITE_OK;

    memset(file, 0, sizeof(WasiFile));
    file->fd = -1;
    file->lock_level = SQLITE_LOCK_NONE;

    /* Convert SQLite flags to POSIX flags */
    if (flags & SQLITE_OPEN_EXCLUSIVE) {
        oflags |= O_EXCL;
    }
    if (flags & SQLITE_OPEN_CREATE) {
        oflags |= O_CREAT;
    }
    if (flags & SQLITE_OPEN_READONLY) {
        oflags |= O_RDONLY;
    } else if (flags & SQLITE_OPEN_READWRITE) {
        oflags |= O_RDWR;
    }

    /* Handle temp/anonymous files */
    if (zName == NULL || zName[0] == '\0') {
        /* Create a temp file name */
        static int temp_counter = 0;
        char temp_name[64];
        sqlite3_snprintf(sizeof(temp_name), temp_name, "/tmp/sqlite_temp_%d", ++temp_counter);
        zName = temp_name;
        oflags |= O_CREAT | O_RDWR;
    }

    /* Open the file */
    file->fd = open(zName, oflags, 0644);
    if (file->fd < 0) {
        g_last_errno = errno;
        return SQLITE_CANTOPEN;
    }

    /* Store path for debugging */
    file->path = sqlite3_mprintf("%s", zName);

    /* Set up I/O methods */
    file->base.pMethods = &g_wasifile_methods;

    if (pOutFlags) {
        *pOutFlags = flags;
    }

    return rc;
}

static int wasivfs_delete(sqlite3_vfs *pVfs, const char *zName, int syncDir) {
    (void)pVfs;
    (void)syncDir;

    if (unlink(zName) != 0) {
        g_last_errno = errno;
        if (errno == ENOENT) {
            return SQLITE_OK; /* File doesn't exist - that's OK */
        }
        return SQLITE_IOERR_DELETE;
    }

    return SQLITE_OK;
}

static int wasivfs_access(sqlite3_vfs *pVfs, const char *zName, int flags, int *pResOut) {
    (void)pVfs;
    int mode = 0;

    switch (flags) {
        case SQLITE_ACCESS_EXISTS:
            mode = F_OK;
            break;
        case SQLITE_ACCESS_READ:
            mode = R_OK;
            break;
        case SQLITE_ACCESS_READWRITE:
            mode = R_OK | W_OK;
            break;
        default:
            *pResOut = 0;
            return SQLITE_OK;
    }

    *pResOut = (access(zName, mode) == 0) ? 1 : 0;
    return SQLITE_OK;
}

static int wasivfs_fullpathname(sqlite3_vfs *pVfs, const char *zName, int nOut, char *zOut) {
    (void)pVfs;

    if (zName == NULL) {
        zOut[0] = '\0';
        return SQLITE_OK;
    }

    /* For WASI, we just use the name as-is since we don't have getcwd */
    int len = (int)strlen(zName);
    if (len >= nOut) {
        return SQLITE_CANTOPEN;
    }

    memcpy(zOut, zName, len + 1);
    return SQLITE_OK;
}

static void *wasivfs_dlopen(sqlite3_vfs *pVfs, const char *zFilename) {
    (void)pVfs;
    (void)zFilename;
    return NULL;
}

static void wasivfs_dlerror(sqlite3_vfs *pVfs, int nByte, char *zErrMsg) {
    (void)pVfs;
    if (nByte > 0) {
        sqlite3_snprintf(nByte, zErrMsg, "Dynamic loading not supported in WASI");
    }
}

static void (*wasivfs_dlsym(sqlite3_vfs *pVfs, void *pHandle, const char *zSymbol))(void) {
    (void)pVfs;
    (void)pHandle;
    (void)zSymbol;
    return NULL;
}

static void wasivfs_dlclose(sqlite3_vfs *pVfs, void *pHandle) {
    (void)pVfs;
    (void)pHandle;
}

static int wasivfs_randomness(sqlite3_vfs *pVfs, int nByte, char *zOut) {
    (void)pVfs;

    /* Try to use /dev/urandom if available */
    int fd = open("/dev/urandom", O_RDONLY);
    if (fd >= 0) {
        ssize_t got = read(fd, zOut, nByte);
        close(fd);
        if (got == nByte) {
            return nByte;
        }
    }

    /* Fallback to time-based PRNG */
    static uint32_t seed = 0;
    if (seed == 0) {
        seed = (uint32_t)time(NULL);
    }

    for (int i = 0; i < nByte; i++) {
        seed = seed * 1103515245 + 12345;
        zOut[i] = (char)(seed >> 16);
    }

    return nByte;
}

static int wasivfs_sleep(sqlite3_vfs *pVfs, int microseconds) {
    (void)pVfs;

    /* WASI preview2 may support this via wasi:clocks/monotonic-clock */
    /* For now, just busy-wait approximately */
    volatile int i;
    for (i = 0; i < microseconds / 10; i++) {
        /* Busy wait */
    }

    return microseconds;
}

static int wasivfs_currenttime(sqlite3_vfs *pVfs, double *pTime) {
    (void)pVfs;

    time_t t = time(NULL);
    /* Convert Unix time to Julian day */
    *pTime = (double)t / 86400.0 + 2440587.5;
    return SQLITE_OK;
}

static int wasivfs_getlasterror(sqlite3_vfs *pVfs, int nBuf, char *zBuf) {
    (void)pVfs;

    if (nBuf > 0 && zBuf) {
        sqlite3_snprintf(nBuf, zBuf, "errno=%d", g_last_errno);
    }

    return g_last_errno;
}

static int wasivfs_currenttimeint64(sqlite3_vfs *pVfs, sqlite3_int64 *pTime) {
    (void)pVfs;

    time_t t = time(NULL);
    /* Convert to milliseconds since Julian epoch */
    *pTime = (sqlite3_int64)t * 1000LL + 210866760000000LL;
    return SQLITE_OK;
}

/*
 * Public API
 */

int sqlite3_wasivfs_register(int makeDefault) {
    static int initialized = 0;

    if (initialized) {
        return SQLITE_OK;
    }

    memset(&g_wasivfs, 0, sizeof(g_wasivfs));

    g_wasivfs.iVersion = 2;
    g_wasivfs.szOsFile = sizeof(WasiFile);
    g_wasivfs.mxPathname = WASIVFS_MAX_PATHNAME;
    g_wasivfs.pNext = NULL;
    g_wasivfs.zName = "wasivfs";
    g_wasivfs.pAppData = NULL;
    g_wasivfs.xOpen = wasivfs_open;
    g_wasivfs.xDelete = wasivfs_delete;
    g_wasivfs.xAccess = wasivfs_access;
    g_wasivfs.xFullPathname = wasivfs_fullpathname;
    g_wasivfs.xDlOpen = wasivfs_dlopen;
    g_wasivfs.xDlError = wasivfs_dlerror;
    g_wasivfs.xDlSym = wasivfs_dlsym;
    g_wasivfs.xDlClose = wasivfs_dlclose;
    g_wasivfs.xRandomness = wasivfs_randomness;
    g_wasivfs.xSleep = wasivfs_sleep;
    g_wasivfs.xCurrentTime = wasivfs_currenttime;
    g_wasivfs.xGetLastError = wasivfs_getlasterror;
    g_wasivfs.xCurrentTimeInt64 = wasivfs_currenttimeint64;

    int rc = sqlite3_vfs_register(&g_wasivfs, makeDefault);
    if (rc == SQLITE_OK) {
        initialized = 1;
    }

    return rc;
}

const char *sqlite3_wasivfs_name(void) {
    return "wasivfs";
}
