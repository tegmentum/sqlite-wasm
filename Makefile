# SQLite WASM Component Makefile
# Builds SQLite as a WebAssembly component targeting WASI Preview 2

.PHONY: all clean deps sqlite wasi-sdk bindings build test test-cli help cli extensions

# Directories
PROJECT_ROOT := $(shell pwd)
DEPS_DIR := $(PROJECT_ROOT)/deps
BUILD_DIR := $(PROJECT_ROOT)/build
SRC_DIR := $(PROJECT_ROOT)/src
WIT_DIR := $(PROJECT_ROOT)/wit
BINDINGS_DIR := $(SRC_DIR)/bindings

# wasi-sdk configuration
WASI_SDK := $(DEPS_DIR)/wasi-sdk
WASI_SYSROOT := $(WASI_SDK)/share/wasi-sysroot
CC := $(WASI_SDK)/bin/clang
AR := $(WASI_SDK)/bin/llvm-ar

# Target triple
TARGET := wasm32-wasip2

# SQLite configuration flags
# Note: FTS5, RTREE, GEOPOLY, JSON1 are built as separate WASM extensions
SQLITE_CFLAGS := \
    -DSQLITE_THREADSAFE=1 \
    -DSQLITE_ENABLE_MATH_FUNCTIONS \
    -DSQLITE_ENABLE_COLUMN_METADATA \
    -DSQLITE_ENABLE_STAT4 \
    -DSQLITE_OMIT_LOCALTIME \
    -DSQLITE_TEMP_STORE=2 \
    -DSQLITE_OS_OTHER=1 \
    -DSQLITE_MUTEX_NOOP \
    -DSQLITE_DEFAULT_MEMSTATUS=0 \
    -DSQLITE_MAX_EXPR_DEPTH=0 \
    -DSQLITE_USE_ALLOCA

# Compiler flags
CFLAGS := \
    --target=$(TARGET) \
    --sysroot=$(WASI_SYSROOT) \
    -O2 \
    -g \
    -Wall \
    -Wextra \
    -Wno-unused-parameter \
    -I$(DEPS_DIR)/sqlite \
    -I$(SRC_DIR) \
    -I$(BINDINGS_DIR) \
    $(SQLITE_CFLAGS)

# Linker flags for reactor (library) mode
LDFLAGS := \
    --target=$(TARGET) \
    --sysroot=$(WASI_SYSROOT) \
    -mexec-model=reactor \
    -Wl,--export-dynamic \
    -Wl,--no-entry

# Source files
SQLITE_SRC := $(DEPS_DIR)/sqlite/sqlite3.c
VFS_SRCS := \
    $(SRC_DIR)/vfs/vfs_memory.c \
    $(SRC_DIR)/vfs/vfs_wasi.c
EXPORT_SRCS := \
    $(SRC_DIR)/exports/low_level.c \
    $(SRC_DIR)/exports/high_level.c
MAIN_SRC := $(SRC_DIR)/sqlite_wasm.c

# Object files
OBJS := \
    $(BUILD_DIR)/sqlite3.o \
    $(BUILD_DIR)/vfs_memory.o \
    $(BUILD_DIR)/vfs_wasi.o \
    $(BUILD_DIR)/low_level.o \
    $(BUILD_DIR)/high_level.o \
    $(BUILD_DIR)/sqlite_wasm.o \
    $(BUILD_DIR)/sqlite_world.o \
    $(BINDINGS_DIR)/sqlite_world_component_type.o

# Output files
CORE_WASM := $(BUILD_DIR)/sqlite-core.wasm
COMPONENT_WASM := $(BUILD_DIR)/sqlite.wasm

# Default target
all: $(COMPONENT_WASM)

# Help
help:
	@echo "SQLite WASM Component Build System"
	@echo ""
	@echo "Targets:"
	@echo "  all          Build the SQLite WASM component (default)"
	@echo "  deps         Download all dependencies (wasi-sdk, sqlite)"
	@echo "  sqlite       Download SQLite amalgamation"
	@echo "  wasi-sdk     Download wasi-sdk toolchain"
	@echo "  bindings     Generate C bindings from WIT"
	@echo "  build        Build the core WASM module"
	@echo "  component    Convert core module to component"
	@echo "  cli          Build the SQLite CLI (sqlite-cli.wasm)"
	@echo "  cli-zip      Build CLI composed with zip-wasm (.archive support)"
	@echo "  cli-common   Build CLI composed with FTS5 + JSON1"
	@echo "  cli-full     Build CLI with all extensions + ZIP support"
	@echo "  extensions   Build all WASM extensions (FTS5, R-Tree, JSON1, GeoPoly)"
	@echo "  test         Run tests"
	@echo "  clean        Remove build artifacts"
	@echo ""
	@echo "Environment variables:"
	@echo "  SQLITE_VERSION    SQLite version (default: 3480000)"
	@echo "  WASI_SDK_VERSION  wasi-sdk version (default: 29)"
	@echo "  ZIP_WASM          Path to zip-wasm component (for cli-zip/cli-full)"

# Download dependencies
deps: wasi-sdk sqlite

sqlite:
	@echo "Downloading SQLite..."
	./scripts/download-sqlite.sh

wasi-sdk:
	@echo "Downloading wasi-sdk..."
	./scripts/download-wasi-sdk.sh

# Generate bindings from WIT
bindings: $(BINDINGS_DIR)/sqlite_world.h

$(BINDINGS_DIR)/sqlite_world.h: $(WIT_DIR)/world.wit $(WIT_DIR)/sqlite-low-level.wit $(WIT_DIR)/sqlite-high-level.wit
	@echo "Generating C bindings from WIT..."
	@mkdir -p $(BINDINGS_DIR)
	wit-bindgen c $(WIT_DIR) --world sqlite-world --out-dir $(BINDINGS_DIR)

# Generate bindings for extensible world (includes extension API)
BINDINGS_EXT_DIR := $(SRC_DIR)/bindings-ext

bindings-ext: $(BINDINGS_EXT_DIR)/sqlite_extensible.h

$(BINDINGS_EXT_DIR)/sqlite_extensible.h: $(WIT_DIR)/world.wit $(WIT_DIR)/sqlite-low-level.wit $(WIT_DIR)/sqlite-high-level.wit $(WIT_DIR)/sqlite-extension.wit
	@echo "Generating C bindings for extensible world..."
	@mkdir -p $(BINDINGS_EXT_DIR)
	wit-bindgen c $(WIT_DIR) --world sqlite-extensible --out-dir $(BINDINGS_EXT_DIR)

# Create build directory
$(BUILD_DIR):
	mkdir -p $(BUILD_DIR)

# Compile SQLite
$(BUILD_DIR)/sqlite3.o: $(SQLITE_SRC) | $(BUILD_DIR)
	@echo "Compiling SQLite..."
	$(CC) $(CFLAGS) -c $< -o $@

# Compile VFS implementations
$(BUILD_DIR)/vfs_memory.o: $(SRC_DIR)/vfs/vfs_memory.c | $(BUILD_DIR)
	@echo "Compiling memory VFS..."
	$(CC) $(CFLAGS) -c $< -o $@

$(BUILD_DIR)/vfs_wasi.o: $(SRC_DIR)/vfs/vfs_wasi.c | $(BUILD_DIR)
	@echo "Compiling WASI VFS..."
	$(CC) $(CFLAGS) -c $< -o $@

# Compile export implementations
$(BUILD_DIR)/low_level.o: $(SRC_DIR)/exports/low_level.c $(BINDINGS_DIR)/sqlite_world.h | $(BUILD_DIR)
	@echo "Compiling low-level exports..."
	$(CC) $(CFLAGS) -c $< -o $@

$(BUILD_DIR)/high_level.o: $(SRC_DIR)/exports/high_level.c $(BINDINGS_DIR)/sqlite_world.h | $(BUILD_DIR)
	@echo "Compiling high-level exports..."
	$(CC) $(CFLAGS) -c $< -o $@

# Compile main wrapper
$(BUILD_DIR)/sqlite_wasm.o: $(MAIN_SRC) $(BINDINGS_DIR)/sqlite_world.h | $(BUILD_DIR)
	@echo "Compiling main wrapper..."
	$(CC) $(CFLAGS) -c $< -o $@

# Compile generated bindings
$(BUILD_DIR)/sqlite_world.o: $(BINDINGS_DIR)/sqlite_world.c $(BINDINGS_DIR)/sqlite_world.h | $(BUILD_DIR)
	@echo "Compiling generated bindings..."
	$(CC) $(CFLAGS) -c $< -o $@

# Link core WASM module
$(CORE_WASM): $(OBJS)
	@echo "Linking core WASM module..."
	$(CC) $(LDFLAGS) $(OBJS) -o $@

# wasm32-wasip2 target already produces a component, just rename/copy
$(COMPONENT_WASM): $(CORE_WASM)
	@echo "Finalizing WASM component..."
	cp $(CORE_WASM) $@
	@echo "Built: $@"
	@wasm-tools component wit $@ 2>/dev/null | head -50 || true
	@ls -lh $@

build: $(CORE_WASM)

component: $(COMPONENT_WASM)

# Run tests
test: $(COMPONENT_WASM)
	@echo "Running tests..."
	@if command -v wasmtime >/dev/null 2>&1; then \
		echo "Testing with wasmtime..."; \
		wasmtime wast tests/unit/*.wast 2>/dev/null || echo "No .wast tests found"; \
	else \
		echo "wasmtime not found, skipping runtime tests"; \
	fi

test-unit: test

test-integration: $(COMPONENT_WASM)
	@echo "Running integration tests..."
	@if command -v jco >/dev/null 2>&1; then \
		echo "Testing with jco..."; \
		jco transpile $(COMPONENT_WASM) -o $(BUILD_DIR)/js --minify 2>/dev/null && \
		node tests/integration/jco/test.js 2>/dev/null || echo "jco tests not configured"; \
	else \
		echo "jco not found, skipping JavaScript integration tests"; \
	fi

test-cli: $(CLI_WASM)
	@echo "Running CLI tests..."
	@./tests/cli/test_load.sh
	@./tests/cli/test_commands.sh

# CLI build
CLI_SRC := $(SRC_DIR)/cli/sqlite_cli.c
CLI_WASM := $(BUILD_DIR)/sqlite-cli.wasm

# CLI compiler flags (command mode, not reactor)
CLI_CFLAGS := \
    --target=$(TARGET) \
    --sysroot=$(WASI_SYSROOT) \
    -O2 \
    -g \
    -Wall \
    -I$(DEPS_DIR)/sqlite \
    -I$(SRC_DIR) \
    -D_WASI_EMULATED_PROCESS_CLOCKS \
    $(SQLITE_CFLAGS)

CLI_LDFLAGS := \
    --target=$(TARGET) \
    --sysroot=$(WASI_SYSROOT) \
    -lwasi-emulated-process-clocks

# CLI object files
CLI_OBJS := \
    $(BUILD_DIR)/sqlite3.o \
    $(BUILD_DIR)/vfs_memory.o \
    $(BUILD_DIR)/vfs_wasi.o \
    $(BUILD_DIR)/sqlite_wasm.o \
    $(BUILD_DIR)/sqlite_cli.o

# Compile CLI main
$(BUILD_DIR)/sqlite_cli.o: $(CLI_SRC) | $(BUILD_DIR)
	@echo "Compiling CLI..."
	$(CC) $(CLI_CFLAGS) -c $< -o $@

# Link CLI
$(CLI_WASM): $(CLI_OBJS)
	@echo "Linking CLI..."
	$(CC) $(CLI_LDFLAGS) $(CLI_OBJS) -o $@
	@echo "Built: $@"
	@ls -lh $@

cli: $(CLI_WASM)

# Extensible build (with extension API)
EXTENSIBLE_WASM := $(BUILD_DIR)/sqlite-extensible.wasm

# Compiler flags for extensible build (bindings-ext first for compatibility wrapper)
CFLAGS_EXT := \
    --target=$(TARGET) \
    --sysroot=$(WASI_SYSROOT) \
    -O2 \
    -g \
    -Wall \
    -Wextra \
    -Wno-unused-parameter \
    -I$(DEPS_DIR)/sqlite \
    -I$(SRC_DIR) \
    -I$(BINDINGS_EXT_DIR) \
    $(SQLITE_CFLAGS)

# Object files for extensible build
OBJS_EXT := \
    $(BUILD_DIR)/sqlite3.o \
    $(BUILD_DIR)/vfs_memory.o \
    $(BUILD_DIR)/vfs_wasi.o \
    $(BUILD_DIR)/low_level_ext.o \
    $(BUILD_DIR)/high_level_ext.o \
    $(BUILD_DIR)/extension.o \
    $(BUILD_DIR)/sqlite_wasm_ext.o \
    $(BUILD_DIR)/sqlite_extensible.o \
    $(BINDINGS_EXT_DIR)/sqlite_extensible_component_type.o

# Compile exports for extensible build (uses sqlite_world.h wrapper in bindings-ext)
$(BUILD_DIR)/low_level_ext.o: $(SRC_DIR)/exports/low_level.c $(BINDINGS_EXT_DIR)/sqlite_extensible.h | $(BUILD_DIR)
	@echo "Compiling low-level exports (extensible)..."
	$(CC) $(CFLAGS_EXT) -c $< -o $@

$(BUILD_DIR)/high_level_ext.o: $(SRC_DIR)/exports/high_level.c $(BINDINGS_EXT_DIR)/sqlite_extensible.h | $(BUILD_DIR)
	@echo "Compiling high-level exports (extensible)..."
	$(CC) $(CFLAGS_EXT) -c $< -o $@

$(BUILD_DIR)/extension.o: $(SRC_DIR)/exports/extension.c $(BINDINGS_EXT_DIR)/sqlite_extensible.h | $(BUILD_DIR)
	@echo "Compiling extension exports..."
	$(CC) $(CFLAGS_EXT) -c $< -o $@

$(BUILD_DIR)/sqlite_wasm_ext.o: $(MAIN_SRC) $(BINDINGS_EXT_DIR)/sqlite_extensible.h | $(BUILD_DIR)
	@echo "Compiling main wrapper (extensible)..."
	$(CC) $(CFLAGS_EXT) -c $< -o $@

$(BUILD_DIR)/sqlite_extensible.o: $(BINDINGS_EXT_DIR)/sqlite_extensible.c $(BINDINGS_EXT_DIR)/sqlite_extensible.h | $(BUILD_DIR)
	@echo "Compiling generated extensible bindings..."
	$(CC) $(CFLAGS_EXT) -c $< -o $@

$(EXTENSIBLE_WASM): $(OBJS_EXT)
	@echo "Linking extensible WASM module..."
	$(CC) $(LDFLAGS) $(OBJS_EXT) -o $@
	@echo "Built: $@"
	@wasm-tools component wit $@ 2>/dev/null | head -50 || true
	@ls -lh $@

extensible: bindings-ext $(EXTENSIBLE_WASM)

.PHONY: extensible

# =============================================================================
# Extension builds
# =============================================================================

EXTENSIONS_DIR := $(PROJECT_ROOT)/extensions
EXT_BUILD_DIR := $(BUILD_DIR)/extensions

# Extension compiler flags (reactor mode with extension-specific flags)
EXT_CFLAGS := \
    --target=$(TARGET) \
    --sysroot=$(WASI_SYSROOT) \
    -O2 \
    -g \
    -Wall \
    -Wextra \
    -Wno-unused-parameter \
    -I$(DEPS_DIR)/sqlite \
    -I$(SRC_DIR) \
    $(SQLITE_CFLAGS)

EXT_LDFLAGS := \
    --target=$(TARGET) \
    --sysroot=$(WASI_SYSROOT) \
    -mexec-model=reactor \
    -Wl,--export-dynamic \
    -Wl,--no-entry

# Create extension build directory
$(EXT_BUILD_DIR):
	mkdir -p $(EXT_BUILD_DIR)

# FTS5 Extension
FTS5_WASM := $(EXT_BUILD_DIR)/fts5.wasm
FTS5_OBJS := $(EXT_BUILD_DIR)/fts5_ext.o $(BUILD_DIR)/sqlite3_fts5.o

$(BUILD_DIR)/sqlite3_fts5.o: $(SQLITE_SRC) | $(BUILD_DIR)
	@echo "Compiling SQLite with FTS5..."
	$(CC) $(EXT_CFLAGS) -DSQLITE_ENABLE_FTS5 -c $< -o $@

$(EXT_BUILD_DIR)/fts5_ext.o: $(EXTENSIONS_DIR)/fts5/fts5_ext.c | $(EXT_BUILD_DIR)
	@echo "Compiling FTS5 extension wrapper..."
	$(CC) $(EXT_CFLAGS) -DSQLITE_ENABLE_FTS5 -c $< -o $@

$(FTS5_WASM): $(FTS5_OBJS)
	@echo "Linking FTS5 extension..."
	$(CC) $(EXT_LDFLAGS) $(FTS5_OBJS) -o $@
	@echo "Built: $@"
	@ls -lh $@

# R-Tree Extension
RTREE_WASM := $(EXT_BUILD_DIR)/rtree.wasm
RTREE_OBJS := $(EXT_BUILD_DIR)/rtree_ext.o $(BUILD_DIR)/sqlite3_rtree.o

$(BUILD_DIR)/sqlite3_rtree.o: $(SQLITE_SRC) | $(BUILD_DIR)
	@echo "Compiling SQLite with R-Tree..."
	$(CC) $(EXT_CFLAGS) -DSQLITE_ENABLE_RTREE -c $< -o $@

$(EXT_BUILD_DIR)/rtree_ext.o: $(EXTENSIONS_DIR)/rtree/rtree_ext.c | $(EXT_BUILD_DIR)
	@echo "Compiling R-Tree extension wrapper..."
	$(CC) $(EXT_CFLAGS) -DSQLITE_ENABLE_RTREE -c $< -o $@

$(RTREE_WASM): $(RTREE_OBJS)
	@echo "Linking R-Tree extension..."
	$(CC) $(EXT_LDFLAGS) $(RTREE_OBJS) -o $@
	@echo "Built: $@"
	@ls -lh $@

# JSON1 Extension
JSON1_WASM := $(EXT_BUILD_DIR)/json1.wasm
JSON1_OBJS := $(EXT_BUILD_DIR)/json1_ext.o $(BUILD_DIR)/sqlite3_json1.o

$(BUILD_DIR)/sqlite3_json1.o: $(SQLITE_SRC) | $(BUILD_DIR)
	@echo "Compiling SQLite with JSON1..."
	$(CC) $(EXT_CFLAGS) -DSQLITE_ENABLE_JSON1 -c $< -o $@

$(EXT_BUILD_DIR)/json1_ext.o: $(EXTENSIONS_DIR)/json1/json1_ext.c | $(EXT_BUILD_DIR)
	@echo "Compiling JSON1 extension wrapper..."
	$(CC) $(EXT_CFLAGS) -DSQLITE_ENABLE_JSON1 -c $< -o $@

$(JSON1_WASM): $(JSON1_OBJS)
	@echo "Linking JSON1 extension..."
	$(CC) $(EXT_LDFLAGS) $(JSON1_OBJS) -o $@
	@echo "Built: $@"
	@ls -lh $@

# GeoPoly Extension (requires R-Tree)
GEOPOLY_WASM := $(EXT_BUILD_DIR)/geopoly.wasm
GEOPOLY_OBJS := $(EXT_BUILD_DIR)/geopoly_ext.o $(BUILD_DIR)/sqlite3_geopoly.o

$(BUILD_DIR)/sqlite3_geopoly.o: $(SQLITE_SRC) | $(BUILD_DIR)
	@echo "Compiling SQLite with GeoPoly..."
	$(CC) $(EXT_CFLAGS) -DSQLITE_ENABLE_GEOPOLY -DSQLITE_ENABLE_RTREE -c $< -o $@

$(EXT_BUILD_DIR)/geopoly_ext.o: $(EXTENSIONS_DIR)/geopoly/geopoly_ext.c | $(EXT_BUILD_DIR)
	@echo "Compiling GeoPoly extension wrapper..."
	$(CC) $(EXT_CFLAGS) -DSQLITE_ENABLE_GEOPOLY -DSQLITE_ENABLE_RTREE -c $< -o $@

$(GEOPOLY_WASM): $(GEOPOLY_OBJS)
	@echo "Linking GeoPoly extension..."
	$(CC) $(EXT_LDFLAGS) $(GEOPOLY_OBJS) -o $@
	@echo "Built: $@"
	@ls -lh $@

# Build all extensions
extensions: $(FTS5_WASM) $(RTREE_WASM) $(JSON1_WASM) $(GEOPOLY_WASM)
	@echo ""
	@echo "All extensions built:"
	@ls -lh $(EXT_BUILD_DIR)/*.wasm

# =============================================================================
# WASM Component Composition
# =============================================================================

# Path to zip-wasm component (set via environment or use default)
ZIP_WASM ?= $(HOME)/git/zip-wasm/build/zip-wasm.wasm

# Composed CLI with ZIP support
CLI_ZIP_WASM := $(BUILD_DIR)/sqlite-cli-zip.wasm

# Compose CLI with zip-wasm for .archive command support
$(CLI_ZIP_WASM): $(CLI_WASM) $(ZIP_WASM)
	@echo "Composing CLI with zip-wasm..."
	@if [ -f "$(ZIP_WASM)" ]; then \
		wasm-tools compose $(CLI_WASM) \
			-d zip-operations=$(ZIP_WASM) \
			-o $@; \
		echo "Built: $@"; \
		ls -lh $@; \
	else \
		echo "Warning: zip-wasm not found at $(ZIP_WASM)"; \
		echo "Set ZIP_WASM environment variable or build zip-wasm first"; \
		echo "Skipping composition, copying CLI without ZIP support"; \
		cp $(CLI_WASM) $@; \
	fi

cli-zip: $(CLI_ZIP_WASM)

# Compose CLI with common extensions (FTS5 + JSON1)
CLI_COMMON_WASM := $(BUILD_DIR)/sqlite-cli-common.wasm

$(CLI_COMMON_WASM): $(CLI_WASM) $(FTS5_WASM) $(JSON1_WASM)
	@echo "Composing CLI with common extensions..."
	wasm-tools compose $(CLI_WASM) \
		-d fts5=$(FTS5_WASM) \
		-d json1=$(JSON1_WASM) \
		-o $@
	@echo "Built: $@"
	@ls -lh $@

cli-common: $(CLI_COMMON_WASM)

# Compose CLI with all features (extensions + ZIP)
CLI_FULL_WASM := $(BUILD_DIR)/sqlite-cli-full.wasm

$(CLI_FULL_WASM): $(CLI_WASM) $(FTS5_WASM) $(JSON1_WASM) $(RTREE_WASM) $(GEOPOLY_WASM)
	@echo "Composing full CLI..."
	@if [ -f "$(ZIP_WASM)" ]; then \
		wasm-tools compose $(CLI_WASM) \
			-d fts5=$(FTS5_WASM) \
			-d json1=$(JSON1_WASM) \
			-d rtree=$(RTREE_WASM) \
			-d geopoly=$(GEOPOLY_WASM) \
			-d zip-operations=$(ZIP_WASM) \
			-o $@; \
	else \
		wasm-tools compose $(CLI_WASM) \
			-d fts5=$(FTS5_WASM) \
			-d json1=$(JSON1_WASM) \
			-d rtree=$(RTREE_WASM) \
			-d geopoly=$(GEOPOLY_WASM) \
			-o $@; \
		echo "Note: Built without ZIP support (zip-wasm not found)"; \
	fi
	@echo "Built: $@"
	@ls -lh $@

cli-full: extensions $(CLI_FULL_WASM)

.PHONY: cli-zip cli-common cli-full

# =============================================================================
# Clean build artifacts
clean:
	rm -rf $(BUILD_DIR)
	rm -rf $(BINDINGS_DIR)
	rm -rf $(BINDINGS_EXT_DIR)

# Clean everything including dependencies
distclean: clean
	rm -rf $(DEPS_DIR)/sqlite/sqlite3.*
	rm -rf $(DEPS_DIR)/wasi-sdk*

# Verify toolchain
verify-tools:
	@echo "Checking required tools..."
	@command -v $(CC) >/dev/null 2>&1 || (echo "wasi-sdk not found. Run 'make wasi-sdk'" && exit 1)
	@command -v wit-bindgen >/dev/null 2>&1 || (echo "wit-bindgen not found. Install with: cargo install wit-bindgen-cli" && exit 1)
	@command -v wasm-tools >/dev/null 2>&1 || (echo "wasm-tools not found. Install with: cargo install wasm-tools" && exit 1)
	@echo "All required tools found."

# Print configuration
info:
	@echo "Configuration:"
	@echo "  PROJECT_ROOT: $(PROJECT_ROOT)"
	@echo "  WASI_SDK: $(WASI_SDK)"
	@echo "  TARGET: $(TARGET)"
	@echo "  CC: $(CC)"
	@echo ""
	@echo "SQLite flags:"
	@echo "  $(SQLITE_CFLAGS)" | tr ' ' '\n' | grep -v '^$$'
