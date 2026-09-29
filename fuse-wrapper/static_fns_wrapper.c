#include "C:\Program Files (x86)\WinFsp\inc\fuse3\fuse.h"
// Static wrappers
int fuse_main_real__extern(int argc, char *argv [], const struct fuse_operations *ops, size_t opsize, void *data) { return fuse_main_real(argc, argv, ops, opsize, data); }
struct fuse_context * fuse_get_context__extern(void) { return fuse_get_context(); }