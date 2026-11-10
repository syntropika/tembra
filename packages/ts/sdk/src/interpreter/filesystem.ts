/**
 * Public browser filesystem provider API. The implementation lives in private modules:
 * `filesystem/contract.ts` (types and errors), `filesystem/scope.ts` (rights, lifetime and path
 * confinement), `filesystem/memory.ts` and `filesystem/opfs.ts` (storage backends).
 */
export {
  type Descriptor,
  type DirectoryDescriptor,
  type DirectoryEntry,
  type EntryType,
  type FileDescriptor,
  type FileStat,
  FilesystemError,
  type FilesystemErrorCode,
  type FilesystemProvider,
  type MountedFilesystem,
  type OpenOptions,
} from "./filesystem/contract.ts";
export { memoryFilesystem } from "./filesystem/memory.ts";
export { opfsFilesystem } from "./filesystem/opfs.ts";
