/*
 * SQLite WASM Memory VFS Implementation
 *
 * Provides an in-memory virtual file system for SQLite.
 * All data is stored in dynamically allocated memory using a chunked approach.
 * Suitable for browser environments and transient database use cases.
 */

#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include "sqlite3.h"

/* Configuration */
#define MEMVFS_CHUNK_SIZE    (64 * 1024)  /* 64KB chunks */
#define MEMVFS_MAX_PATHNAME  512

/* Forward declarations */
typedef struct MemFile MemFile;
typedef struct MemVfs MemVfs;

/* Chunk structure for storing file data */
typedef struct MemChunk {
    struct MemChunk *next;
    uint8_t data[MEMVFS_CHUNK_SIZE];
} MemChunk;

/* In-memory file structure */
struct MemFile {
    sqlite3_file base;      /* Base class - must be first */
    char *name;             /* File name (for debugging) */
    MemChunk *chunks;       /* Linked list of data chunks */
    sqlite3_int64 size;     /* Current file size */
    int ref_count;          /* Reference count for shared files */
    MemFile *next;          /* Next file in global list */
};

/* VFS structure */
struct MemVfs {
    sqlite3_vfs base;       /* Base class - must be first */
    MemFile *files;         /* List of all open files */
};

/* Global VFS instance */
static MemVfs g_memvfs;

/* Forward declarations of VFS methods */
static int memvfs_open(sqlite3_vfs*, const char*, sqlite3_file*, int, int*);
static int memvfs_delete(sqlite3_vfs*, const char*, int);
static int memvfs_access(sqlite3_vfs*, const char*, int, int*);
static int memvfs_fullpathname(sqlite3_vfs*, const char*, int, char*);
static void *memvfs_dlopen(sqlite3_vfs*, const char*);
static void memvfs_dlerror(sqlite3_vfs*, int, char*);
static void (*memvfs_dlsym(sqlite3_vfs*, void*, const char*))(void);
static void memvfs_dlclose(sqlite3_vfs*, void*);
static int memvfs_randomness(sqlite3_vfs*, int, char*);
static int memvfs_sleep(sqlite3_vfs*, int);
static int memvfs_currenttime(sqlite3_vfs*, double*);
static int memvfs_getlasterror(sqlite3_vfs*, int, char*);
static int memvfs_currenttimeint64(sqlite3_vfs*, sqlite3_int64*);

/* Forward declarations of file methods */
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

/* I/O methods structure */
static const sqlite3_io_methods g_memfile_methods = {
    1,                          /* iVersion */
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
    /* Version 2+ methods */
    NULL,                       /* xShmMap */
    NULL,                       /* xShmLock */
    NULL,                       /* xShmBarrier */
    NULL,                       /* xShmUnmap */
    /* Version 3+ methods */
    NULL,                       /* xFetch */
    NULL                        /* xUnfetch */
};

/* Helper: Find a file by name */
static MemFile *memvfs_find_file(const char *name) {
    MemFile *f = g_memvfs.files;
    while (f) {
        if (f->name && strcmp(f->name, name) == 0) {
            return f;
        }
        f = f->next;
    }
    return NULL;
}

/* Helper: Get chunk containing offset, creating if needed */
static MemChunk *memfile_get_chunk(MemFile *file, sqlite3_int64 offset, int create) {
    sqlite3_int64 chunk_idx = offset / MEMVFS_CHUNK_SIZE;
    MemChunk **pp = &file->chunks;
    sqlite3_int64 i;

    for (i = 0; i < chunk_idx; i++) {
        if (*pp == NULL) {
            if (!create) return NULL;
            *pp = (MemChunk*)sqlite3_malloc(sizeof(MemChunk));
            if (*pp == NULL) return NULL;
            memset(*pp, 0, sizeof(MemChunk));
        }
        pp = &(*pp)->next;
    }

    if (*pp == NULL) {
        if (!create) return NULL;
        *pp = (MemChunk*)sqlite3_malloc(sizeof(MemChunk));
        if (*pp == NULL) return NULL;
        memset(*pp, 0, sizeof(MemChunk));
    }

    return *pp;
}

/* Helper: Free all chunks */
static void memfile_free_chunks(MemFile *file) {
    MemChunk *chunk = file->chunks;
    while (chunk) {
        MemChunk *next = chunk->next;
        sqlite3_free(chunk);
        chunk = next;
    }
    file->chunks = NULL;
    file->size = 0;
}

/*
 * File method implementations
 */

static int memfile_close(sqlite3_file *pFile) {
    MemFile *file = (MemFile*)pFile;

    file->ref_count--;
    if (file->ref_count <= 0) {
        /* Remove from global list */
        MemFile **pp = &g_memvfs.files;
        while (*pp && *pp != file) {
            pp = &(*pp)->next;
        }
        if (*pp) {
            *pp = file->next;
        }

        /* Free resources */
        memfile_free_chunks(file);
        if (file->name) {
            sqlite3_free(file->name);
        }
    }

    return SQLITE_OK;
}

static int memfile_read(sqlite3_file *pFile, void *buf, int amt, sqlite3_int64 offset) {
    MemFile *file = (MemFile*)pFile;
    uint8_t *dst = (uint8_t*)buf;
    int remaining = amt;

    /* Check for read past end of file */
    if (offset >= file->size) {
        memset(buf, 0, amt);
        return SQLITE_IOERR_SHORT_READ;
    }

    /* Limit read to file size */
    if (offset + amt > file->size) {
        remaining = (int)(file->size - offset);
        memset(dst + remaining, 0, amt - remaining);
    }

    /* Read from chunks */
    while (remaining > 0) {
        sqlite3_int64 chunk_offset = offset % MEMVFS_CHUNK_SIZE;
        int to_read = MEMVFS_CHUNK_SIZE - (int)chunk_offset;
        if (to_read > remaining) to_read = remaining;

        MemChunk *chunk = memfile_get_chunk(file, offset, 0);
        if (chunk) {
            memcpy(dst, chunk->data + chunk_offset, to_read);
        } else {
            memset(dst, 0, to_read);
        }

        dst += to_read;
        offset += to_read;
        remaining -= to_read;
    }

    return (remaining == 0) ? SQLITE_OK : SQLITE_IOERR_SHORT_READ;
}

static int memfile_write(sqlite3_file *pFile, const void *buf, int amt, sqlite3_int64 offset) {
    MemFile *file = (MemFile*)pFile;
    const uint8_t *src = (const uint8_t*)buf;
    int remaining = amt;

    /* Write to chunks */
    while (remaining > 0) {
        sqlite3_int64 chunk_offset = offset % MEMVFS_CHUNK_SIZE;
        int to_write = MEMVFS_CHUNK_SIZE - (int)chunk_offset;
        if (to_write > remaining) to_write = remaining;

        MemChunk *chunk = memfile_get_chunk(file, offset, 1);
        if (chunk == NULL) {
            return SQLITE_NOMEM;
        }

        memcpy(chunk->data + chunk_offset, src, to_write);

        src += to_write;
        offset += to_write;
        remaining -= to_write;
    }

    /* Update file size */
    sqlite3_int64 new_end = offset;
    if (new_end > file->size) {
        file->size = new_end;
    }

    return SQLITE_OK;
}

static int memfile_truncate(sqlite3_file *pFile, sqlite3_int64 size) {
    MemFile *file = (MemFile*)pFile;

    if (size < 0) {
        return SQLITE_IOERR_TRUNCATE;
    }

    /* Free chunks beyond the new size */
    if (size < file->size) {
        sqlite3_int64 keep_chunks = (size + MEMVFS_CHUNK_SIZE - 1) / MEMVFS_CHUNK_SIZE;
        MemChunk *chunk = file->chunks;
        sqlite3_int64 i;

        for (i = 0; i < keep_chunks && chunk; i++) {
            chunk = chunk->next;
        }

        /* Free remaining chunks */
        while (chunk) {
            MemChunk *next = chunk->next;
            sqlite3_free(chunk);
            chunk = next;
        }

        /* Truncate the list */
        if (keep_chunks == 0) {
            file->chunks = NULL;
        } else {
            chunk = file->chunks;
            for (i = 1; i < keep_chunks && chunk; i++) {
                chunk = chunk->next;
            }
            if (chunk) {
                chunk->next = NULL;
            }
        }
    }

    file->size = size;
    return SQLITE_OK;
}

static int memfile_sync(sqlite3_file *pFile, int flags) {
    (void)pFile;
    (void)flags;
    /* No-op for memory - data is always "synced" */
    return SQLITE_OK;
}

static int memfile_filesize(sqlite3_file *pFile, sqlite3_int64 *pSize) {
    MemFile *file = (MemFile*)pFile;
    *pSize = file->size;
    return SQLITE_OK;
}

static int memfile_lock(sqlite3_file *pFile, int level) {
    (void)pFile;
    (void)level;
    /* No-op for memory - single process */
    return SQLITE_OK;
}

static int memfile_unlock(sqlite3_file *pFile, int level) {
    (void)pFile;
    (void)level;
    /* No-op for memory - single process */
    return SQLITE_OK;
}

static int memfile_checkreservedlock(sqlite3_file *pFile, int *pResOut) {
    (void)pFile;
    *pResOut = 0;
    return SQLITE_OK;
}

static int memfile_filecontrol(sqlite3_file *pFile, int op, void *pArg) {
    (void)pFile;
    (void)op;
    (void)pArg;
    return SQLITE_NOTFOUND;
}

static int memfile_sectorsize(sqlite3_file *pFile) {
    (void)pFile;
    return 4096;
}

static int memfile_devicecharacteristics(sqlite3_file *pFile) {
    (void)pFile;
    return SQLITE_IOCAP_ATOMIC |
           SQLITE_IOCAP_SAFE_APPEND |
           SQLITE_IOCAP_SEQUENTIAL |
           SQLITE_IOCAP_POWERSAFE_OVERWRITE;
}

/*
 * VFS method implementations
 */

static int memvfs_open(sqlite3_vfs *pVfs, const char *zName, sqlite3_file *pFile,
                       int flags, int *pOutFlags) {
    (void)pVfs;
    MemFile *file = (MemFile*)pFile;

    memset(file, 0, sizeof(MemFile));
    file->base.pMethods = &g_memfile_methods;
    file->ref_count = 1;

    /* Handle anonymous temp files */
    if (zName == NULL || zName[0] == '\0') {
        file->name = NULL;
    } else {
        /* Check for existing file */
        MemFile *existing = memvfs_find_file(zName);
        if (existing) {
            /* Open existing file */
            if (flags & SQLITE_OPEN_EXCLUSIVE) {
                return SQLITE_CANTOPEN;
            }
            existing->ref_count++;
            memcpy(file, existing, sizeof(MemFile));
            if (pOutFlags) {
                *pOutFlags = flags;
            }
            return SQLITE_OK;
        }

        /* Create new file */
        file->name = sqlite3_mprintf("%s", zName);
        if (file->name == NULL) {
            return SQLITE_NOMEM;
        }
    }

    file->chunks = NULL;
    file->size = 0;

    /* Add to global list if named */
    if (file->name) {
        file->next = g_memvfs.files;
        g_memvfs.files = file;
    }

    if (pOutFlags) {
        *pOutFlags = flags;
    }

    return SQLITE_OK;
}

static int memvfs_delete(sqlite3_vfs *pVfs, const char *zName, int syncDir) {
    (void)pVfs;
    (void)syncDir;

    MemFile *file = memvfs_find_file(zName);
    if (file) {
        /* Remove from list */
        MemFile **pp = &g_memvfs.files;
        while (*pp && *pp != file) {
            pp = &(*pp)->next;
        }
        if (*pp) {
            *pp = file->next;
        }

        /* Free resources */
        memfile_free_chunks(file);
        if (file->name) {
            sqlite3_free(file->name);
        }
    }

    return SQLITE_OK;
}

static int memvfs_access(sqlite3_vfs *pVfs, const char *zName, int flags, int *pResOut) {
    (void)pVfs;

    MemFile *file = memvfs_find_file(zName);

    switch (flags) {
        case SQLITE_ACCESS_EXISTS:
            *pResOut = (file != NULL);
            break;
        case SQLITE_ACCESS_READ:
        case SQLITE_ACCESS_READWRITE:
            *pResOut = (file != NULL);
            break;
        default:
            *pResOut = 0;
            break;
    }

    return SQLITE_OK;
}

static int memvfs_fullpathname(sqlite3_vfs *pVfs, const char *zName, int nOut, char *zOut) {
    (void)pVfs;

    if (zName == NULL) {
        zOut[0] = '\0';
        return SQLITE_OK;
    }

    int len = (int)strlen(zName);
    if (len >= nOut) {
        return SQLITE_CANTOPEN;
    }

    memcpy(zOut, zName, len + 1);
    return SQLITE_OK;
}

static void *memvfs_dlopen(sqlite3_vfs *pVfs, const char *zFilename) {
    (void)pVfs;
    (void)zFilename;
    return NULL;
}

static void memvfs_dlerror(sqlite3_vfs *pVfs, int nByte, char *zErrMsg) {
    (void)pVfs;
    if (nByte > 0) {
        sqlite3_snprintf(nByte, zErrMsg, "Dynamic loading not supported");
    }
}

static void (*memvfs_dlsym(sqlite3_vfs *pVfs, void *pHandle, const char *zSymbol))(void) {
    (void)pVfs;
    (void)pHandle;
    (void)zSymbol;
    return NULL;
}

static void memvfs_dlclose(sqlite3_vfs *pVfs, void *pHandle) {
    (void)pVfs;
    (void)pHandle;
}

static int memvfs_randomness(sqlite3_vfs *pVfs, int nByte, char *zOut) {
    (void)pVfs;

    /* Simple PRNG for when WASI random is not available */
    static uint32_t seed = 0x12345678;
    int i;

    for (i = 0; i < nByte; i++) {
        seed = seed * 1103515245 + 12345;
        zOut[i] = (char)(seed >> 16);
    }

    return nByte;
}

static int memvfs_sleep(sqlite3_vfs *pVfs, int microseconds) {
    (void)pVfs;
    (void)microseconds;
    /* Sleep not supported in basic WASM - just return */
    return 0;
}

static int memvfs_currenttime(sqlite3_vfs *pVfs, double *pTime) {
    (void)pVfs;
    /* Return a fixed time - real implementation would use WASI clocks */
    *pTime = 2460000.5; /* Julian day for a reasonable date */
    return SQLITE_OK;
}

static int memvfs_getlasterror(sqlite3_vfs *pVfs, int nBuf, char *zBuf) {
    (void)pVfs;
    if (nBuf > 0) {
        zBuf[0] = '\0';
    }
    return 0;
}

static int memvfs_currenttimeint64(sqlite3_vfs *pVfs, sqlite3_int64 *pTime) {
    (void)pVfs;
    /* Return milliseconds since Julian epoch */
    *pTime = 2460000LL * 86400000LL;
    return SQLITE_OK;
}

/*
 * Public API
 */

int sqlite3_memvfs_register(int makeDefault) {
    static int initialized = 0;

    if (initialized) {
        return SQLITE_OK;
    }

    memset(&g_memvfs, 0, sizeof(g_memvfs));

    g_memvfs.base.iVersion = 2;
    g_memvfs.base.szOsFile = sizeof(MemFile);
    g_memvfs.base.mxPathname = MEMVFS_MAX_PATHNAME;
    g_memvfs.base.pNext = NULL;
    g_memvfs.base.zName = "memvfs";
    g_memvfs.base.pAppData = NULL;
    g_memvfs.base.xOpen = memvfs_open;
    g_memvfs.base.xDelete = memvfs_delete;
    g_memvfs.base.xAccess = memvfs_access;
    g_memvfs.base.xFullPathname = memvfs_fullpathname;
    g_memvfs.base.xDlOpen = memvfs_dlopen;
    g_memvfs.base.xDlError = memvfs_dlerror;
    g_memvfs.base.xDlSym = memvfs_dlsym;
    g_memvfs.base.xDlClose = memvfs_dlclose;
    g_memvfs.base.xRandomness = memvfs_randomness;
    g_memvfs.base.xSleep = memvfs_sleep;
    g_memvfs.base.xCurrentTime = memvfs_currenttime;
    g_memvfs.base.xGetLastError = memvfs_getlasterror;
    g_memvfs.base.xCurrentTimeInt64 = memvfs_currenttimeint64;

    g_memvfs.files = NULL;

    int rc = sqlite3_vfs_register(&g_memvfs.base, makeDefault);
    if (rc == SQLITE_OK) {
        initialized = 1;
    }

    return rc;
}

const char *sqlite3_memvfs_name(void) {
    return "memvfs";
}
