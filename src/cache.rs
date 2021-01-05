//! Source cache.

use crate::error::{Error, ImportError, ParseError, TypecheckError};
use crate::identifier::Ident;
use crate::parser::lexer::Lexer;
use crate::position::RawSpan;
use crate::term::{RichTerm, Term};
use crate::typecheck::type_check;
use crate::{eval, parser, transformations};
use codespan::{FileId, Files};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::result::Result;
use std::time::SystemTime;

/// File and terms cache.
///
/// Manage a file database, which stores a set of sources (the original source code as string) and
/// the corresponding parsed terms. The storage comprises three elements:
///
/// - the file database, holding the string content of sources indexed by unique `FileId`.
/// identifiers
/// - the name-id table, associating source names for standalone inputs, or paths and timestamps
/// for files, to `FileId`s
/// - the term cache, holding parsed terms indexed by `FileId`s
///
/// Terms possibly undergo typechecking and program transformation. The state of each entry (that
/// is, the operations that have been performed on this term) is stored in an
/// [`EntryState`](./enum.EntryState.html).
#[derive(Debug, Clone)]
pub struct Cache {
    /// The content of the program sources plus imports.
    files: Files<String>,
    /// The name-id table, holding file ids stored in the database indexed by source names.
    file_ids: HashMap<OsString, NameIdEntry>,
    /// Cache storing parsed terms corresponding to the entries of the file database.
    terms: HashMap<FileId, (RichTerm, EntryState)>,
}

/// Cache keys for sources.
///
/// A source can be either a snippet input by the user, in which case it is uniquely identified by
/// its name that corresponds to a unique `FileId`. On the other hand, different versions of the
/// same file can coexist during the same session of the REPL. For this reason, an entry of the
/// name-id table of a file also stores the *modified at* timestamp, such that if a file is
/// imported or loaded again and has been modified in between, the entry is invalidated, the
/// content is loaded again and a new `FileId` is generated.
///
/// Note that in that case, invalidation just means that the `FileId` of a previous version is not
/// accessible anymore just using the name of a file. However, terms that contain non evaluated
/// imports or source locations referring to previous version are still able access the
/// corresponding source or term by using the corresponding `FileId`, which are kept respectively
/// in `files` and `cache`.
#[derive(Eq, PartialEq, Ord, PartialOrd, Debug, Copy, Clone)]
pub struct NameIdEntry {
    id: FileId,
    timestamp: Option<SystemTime>,
}

/// The state of an entry of the term cache.
#[derive(Eq, PartialEq, Ord, PartialOrd, Debug, Copy, Clone)]
pub enum EntryState {
    /// The term have just been parsed.
    Parsed,
    /// The term have been parsed and typechecked.
    Typechecked,
    /// The term have been parsed, possibly typechecked (but not necessarily), and transformed.
    Transformed,
}

/// The result of a cache operation, such as parsing, typechecking, etc. which can either have
/// performed actual work, or have done nothing if the corresponding entry was already in at a
/// later stage.
#[derive(Eq, PartialEq, Ord, PartialOrd, Debug, Copy, Clone)]
pub enum CacheOp {
    Done,
    Cached,
}

/// Wrapper around other errors to indicate that typechecking or applying program transformations
/// failed because the source has not been parsed yet.
pub enum CacheError<E> {
    Error(E),
    NotParsed,
}

impl<E> From<E> for CacheError<E> {
    fn from(e: E) -> Self {
        CacheError::Error(e)
    }
}

impl<E> CacheError<E> {
    pub fn expect_err(self, msg: &str) -> E {
        match self {
            CacheError::Error(err) => err,
            CacheError::NotParsed => panic!("{}", msg),
        }
    }
}

/// Return status indicating if an import has been resolved from a file (first encounter), or was
/// retrieved from the cache.
///
/// See [`resolve`](./fn.resolve.html).
#[derive(Debug, PartialEq)]
pub enum ResolvedTerm {
    FromFile {
        term: RichTerm, /* the parsed term */
        path: PathBuf,  /* the loaded path */
    },
    FromCache(),
}

impl Cache {
    pub fn new() -> Self {
        Cache {
            files: Files::new(),
            file_ids: HashMap::new(),
            terms: HashMap::new(),
        }
    }

    /// Load a file in the file database. Do not insert an entry in the name-id table.
    fn load_file(&mut self, path: impl Into<OsString>) -> std::io::Result<FileId> {
        let path = path.into();
        let mut buffer = String::new();
        fs::File::open(&path)
            .and_then(|mut file| file.read_to_string(&mut buffer))
            .map(|_| self.files.add(path, buffer))
    }

    /// Same as [`add_file`](./fn.add_file.html), but assume that the path is already normalized.
    fn add_file_normalized(&mut self, path: impl Into<OsString>) -> std::io::Result<FileId> {
        let path = path.into();
        let timestamp = Some(fs::metadata(&path)?.modified()?);
        let file_id = self.load_file(path.clone())?;
        self.file_ids.insert(
            path,
            NameIdEntry {
                id: file_id,
                timestamp,
            },
        );
        Ok(file_id)
    }

    /// Load a file and add it to the name-id table.
    ///
    /// Use the normalized path and the *modified at* timestamp as the name-id table entry. Do not
    /// check if a source with the same name as the normalized path of the file and the same
    /// *modified at* timestamp already exists: if it is the case, this one will override the old
    /// entry in the table.
    ///
    /// If the path cannot be normalized because of an IO error, then the file is loaded but not added to the
    /// name-id table.
    pub fn add_file(&mut self, path: impl Into<OsString>) -> std::io::Result<FileId> {
        let path = path.into();
        match normalize_path(PathBuf::from(&path).as_path()) {
            Some(p) => self.add_file_normalized(&p),
            None => self.load_file(path),
        }
    }

    /// Load a source and add it to the name-id table.
    ///
    /// Do not check if a source with the same name already exists: if it is the
    /// case, this one will override the old entry in the name-id table.
    pub fn add_source<T, S>(&mut self, source_name: S, mut source: T) -> std::io::Result<FileId>
    where
        T: Read,
        S: Into<OsString>,
    {
        let mut buffer = String::new();
        source.read_to_string(&mut buffer)?;
        Ok(self.add_string(source_name, buffer))
    }

    /// Load a new source as a string and add it to the name-id table.
    ///
    /// Do not check if a source with the same name already exists: if it is the case, this one
    /// will override the old entry in the name-id table.
    pub fn add_string(&mut self, source_name: impl Into<OsString>, s: String) -> FileId {
        let source_name = source_name.into();
        let id = self.files.add(source_name.clone(), s);
        self.file_ids.insert(
            source_name,
            NameIdEntry {
                id,
                timestamp: None,
            },
        );
        id
    }

    /// Parse a source file and populate the corresponding entry in the cache, or just get it from
    /// the term cache if it is there.
    pub fn parse(&mut self, file_id: FileId) -> Result<CacheOp, ParseError> {
        if self.terms.contains_key(&file_id) {
            Ok(CacheOp::Cached)
        } else {
            let buf = self.files.source(file_id).clone();
            let t = parser::grammar::TermParser::new()
                .parse(file_id, Lexer::new(&buf))
                .map_err(|err| ParseError::from_lalrpop(err, file_id))?;
            self.terms.insert(file_id, (t, EntryState::Parsed));
            Ok(CacheOp::Done)
        }
    }

    /// Typecheck an entry of the cache and update its state accordingly, or do nothing if the
    /// entry has already been typechecked. Require that the corresponding source has been parsed.
    pub fn typecheck(
        &mut self,
        file_id: FileId,
        global_env: &eval::Environment,
    ) -> Result<CacheOp, CacheError<TypecheckError>> {
        if !self.terms.contains_key(&file_id) {
            return Err(CacheError::NotParsed);
        }

        // After self.parse(), the cache must be populated
        let (t, state) = self.terms.get(&file_id).unwrap();

        if *state > EntryState::Typechecked {
            Ok(CacheOp::Cached)
        } else if *state == EntryState::Parsed {
            type_check(t, global_env, self)?;
            self.update_state(file_id, EntryState::Typechecked);
            Ok(CacheOp::Done)
        } else {
            panic!()
        }
    }

    /// Apply program transformations to an entry of the cache, and update its state accordingly,
    /// or do nothing if the entry has already been transformed.
    pub fn transform(&mut self, file_id: FileId) -> Result<CacheOp, CacheError<ImportError>> {
        match self.entry_state(file_id) {
            Some(EntryState::Transformed) => Ok(CacheOp::Cached),
            Some(_) => {
                let (t, _) = self.terms.remove(&file_id).unwrap();
                let t = transformations::transform(t, self)?;
                self.terms.insert(file_id, (t, EntryState::Transformed));
                Ok(CacheOp::Done)
            }
            None => Err(CacheError::NotParsed),
        }
    }

    /// Apply program transformations to all the field of a record. Used to transform the standard
    /// library.
    pub fn transform_inner(&mut self, file_id: FileId) -> Result<CacheOp, CacheError<ImportError>> {
        match self.entry_state(file_id) {
            Some(EntryState::Transformed) => Ok(CacheOp::Cached),
            Some(_) => {
                let (mut t, _) = self.terms.remove(&file_id).unwrap();
                match t.term.as_mut() {
                    Term::Record(ref mut map) | Term::RecRecord(ref mut map) => {
                        let map_res: Result<HashMap<Ident, RichTerm>, ImportError> =
                            std::mem::replace(map, HashMap::new())
                                .into_iter()
                                .map(|(id, t)| {
                                    transformations::transform(t, self)
                                        .map(|t_ok| (id.clone(), t_ok))
                                })
                                .collect();
                        std::mem::replace(map, map_res?);
                    }
                    _ => panic!("cache::transform_inner(): not a record"),
                }

                self.terms.insert(file_id, (t, EntryState::Transformed));
                Ok(CacheOp::Done)
            }
            None => Err(CacheError::NotParsed),
        }
    }

    /// Prepare a source for evaluation: parse it, typecheck it and apply program transformations,
    /// if it was not already done.
    pub fn prepare(
        &mut self,
        file_id: FileId,
        global_env: &eval::Environment,
    ) -> Result<CacheOp, Error> {
        let mut result = CacheOp::Cached;

        if self.parse(file_id)? == CacheOp::Done {
            result = CacheOp::Done;
        };

        let typecheck_res = self.typecheck(file_id, global_env).map_err(|cache_err| {
            cache_err
                .expect_err("cache::prepare(): expected source to be parsed before typechecking")
        })?;
        if typecheck_res == CacheOp::Done {
            result = CacheOp::Done;
        };

        let transform_res = self.transform(file_id).map_err(|cache_err| {
            cache_err
                .expect_err("cache::prepare(): expected source to be parsed before transformations")
        })?;
        if transform_res == CacheOp::Done {
            result = CacheOp::Done;
        };

        Ok(result)
    }

    /// Retrieve the name of a source given an id.
    pub fn name(&self, file_id: FileId) -> &OsStr {
        self.files.name(file_id)
    }

    /// Retrieve the id of a source given a name.
    ///
    /// Note that files added via [`add_file`](fn.add_file.html) are indexed by their full
    /// normalized path (cf [`normalize_path`](./fn.normalize_path.html)). When querying file,
    /// rather use [`id_entry`](./fn.id_entry).
    pub fn id_of(&self, name: impl AsRef<OsStr>) -> Option<FileId> {
        self.file_ids.get(name.as_ref()).map(|entry| entry.id)
    }

    /// Get a mutable reference to the underlying files. Required by the `to_diagnostic` method of
    /// errors.
    pub fn files_mut<'a>(&'a mut self) -> &'a mut Files<String> {
        &mut self.files
    }

    /// Update the state of an entry. Return the previous state.
    pub fn update_state(&mut self, file_id: FileId, new: EntryState) -> Option<EntryState> {
        self.terms
            .get_mut(&file_id)
            .map(|(_, old)| std::mem::replace(old, new))
    }

    /// Retrieve the state of an entry. Return `None` if the entry is not in the term cache,
    /// meaning that the content of the source has been loaded but has not been parsed yet.
    pub fn entry_state(&self, file_id: FileId) -> Option<EntryState> {
        self.terms.get(&file_id).map(|(_, state)| state).copied()
    }

    /// Retrieve a fresh clone of a cached term.
    pub fn get_owned(&self, file_id: FileId) -> Option<RichTerm> {
        self.terms.get(&file_id).map(|(t, _)| t.clone())
    }
}

/// Abstract the access to imported files and the import cache. Used by the evaluator, the
/// typechecker and at [import resolution](../transformations/import_resolution/index.html) phase.
///
/// The standard implementation use 2 caches, the file cache for raw contents and the term cache
/// for parsed contents, mirroring the 2 steps when resolving an import:
/// 1. When an import is encountered for the first time, the content of the corresponding file is
///    read and stored in the file cache (consisting of the file database plus a map between paths
///    and ids in the database). The content is parsed, and this term is queued somewhere so that
///    it can undergo the standard [transformations](../transformations/index.html) first, but is
///    not stored in the term cache yet.
/// 2. When it is finally processed, the term cache is updated with the transformed term.
pub trait ImportResolver {
    /// Resolve an import.
    ///
    /// Read and store the content of an import, put it in the file cache (or get it from there if
    /// it is cached), then parse it and return the corresponding term and file id.
    ///
    /// The term and the path are provided only if the import is processed for the first time.
    /// Indeed, at import resolution phase, the term of an import encountered for the first time is
    /// queued to be processed (e.g. having its own imports resolved). The path is needed to
    /// resolve nested imports relatively to this parent. Only after this processing the term is
    /// inserted back in the cache via [`insert`](#method.insert). On the other hand, if it has
    /// been resolved before, it is already transformed in the cache and do not need further
    /// processing.
    fn resolve(
        &mut self,
        path: &OsStr,
        parent: Option<PathBuf>,
        pos: &Option<RawSpan>,
    ) -> Result<(ResolvedTerm, FileId), ImportError>;

    /// Insert an entry in the term cache after transformation.
    fn insert(&mut self, file_id: FileId, term: RichTerm);

    /// Get a resolved import from the term cache.
    fn get(&self, file_id: FileId) -> Option<RichTerm>;

    /// Get a file id from the file cache.
    fn get_id(&self, path: &OsStr, parent: Option<PathBuf>) -> Option<FileId>;
}

impl ImportResolver for Cache {
    fn resolve(
        &mut self,
        path: &OsStr,
        parent: Option<PathBuf>,
        pos: &Option<RawSpan>,
    ) -> Result<(ResolvedTerm, FileId), ImportError> {
        let (path_buf, normalized) = with_parent(path, parent);

        if let Some(id) = normalized.as_ref().and_then(|p| self.id_of(p)) {
            return Ok((ResolvedTerm::FromCache(), id));
        }

        let file_id = normalized
            .map(|p| self.add_file_normalized(p))
            .unwrap_or_else(|| self.load_file(path_buf))
            .map_err(|err| {
                ImportError::IOError(
                    path.to_string_lossy().into_owned(),
                    format!("{}", err),
                    pos.clone(),
                )
            })?;

        self.parse(file_id)
            .map_err(|err| ImportError::ParseError(err, pos.clone()))?;
        Ok((
            ResolvedTerm::FromFile {
                term: self.get_owned(file_id).unwrap(),
                path: Path::new(path).to_path_buf(),
            },
            file_id,
        ))
    }

    fn get(&self, file_id: FileId) -> Option<RichTerm> {
        self.terms.get(&file_id).map(|(term, state)| {
            debug_assert!(*state == EntryState::Transformed);
            term.clone()
        })
    }

    fn get_id(&self, path: &OsStr, parent: Option<PathBuf>) -> Option<FileId> {
        let (_, normalized) = with_parent(path, parent);
        normalized
            .and_then(|p| self.file_ids.get(&p))
            .map(|entry| entry.id)
    }

    fn insert(&mut self, file_id: FileId, term: RichTerm) {
        self.terms.insert(file_id, (term, EntryState::Transformed));
    }
}

/// Compute the normalized path of a file relatively to a parent (see
/// [`normalize_path`](./fn.normalize_path.html)).
fn with_parent(path: &OsStr, parent: Option<PathBuf>) -> (PathBuf, Option<OsString>) {
    let mut path_buf = parent.unwrap_or(PathBuf::new());
    path_buf.pop();
    path_buf.push(Path::new(path));
    let normalized = normalize_path(path_buf.as_path());

    (path_buf, normalized)
}

/// Normalize the path of a file to uniquely identify names in the cache.
///
/// If an IO error occurs here, `None` is returned.
pub fn normalize_path(path: &Path) -> Option<OsString> {
    path.canonicalize()
        .ok()
        .map(|p_| p_.as_os_str().to_os_string())
}

/// Provide mockup import resolvers for testing purpose.
#[cfg(test)]
pub mod resolvers {
    use super::*;

    /// A dummy resolver that panics when asked to do something. Used to test code that contains no
    /// import.
    pub struct DummyResolver {}

    impl ImportResolver for DummyResolver {
        fn resolve(
            &mut self,
            _path: &OsStr,
            _parent: Option<PathBuf>,
            _pos: &Option<RawSpan>,
        ) -> Result<(ResolvedTerm, FileId), ImportError> {
            panic!("program::resolvers: dummy resolver should not have been invoked");
        }

        fn insert(&mut self, _file_id: FileId, _term: RichTerm) {
            panic!("program::resolvers: dummy resolver should not have been invoked");
        }

        fn get(&self, _file_id: FileId) -> Option<RichTerm> {
            panic!("program::resolvers: dummy resolver should not have been invoked");
        }

        fn get_id(&self, _path: &OsStr, _parent: Option<PathBuf>) -> Option<FileId> {
            panic!("program::resolvers: dummy resolver should not have been invoked");
        }
    }

    /// Resolve imports from a mockup file database. Used to test imports without accessing the
    /// file system. File name are stored as strings, and silently converted from/to `OsString`
    /// when needed: don't use this resolver with source code that import non UTF-8 paths.
    pub struct SimpleResolver {
        files: Files<String>,
        file_cache: HashMap<String, FileId>,
        term_cache: HashMap<FileId, Option<RichTerm>>,
    }

    impl SimpleResolver {
        pub fn new() -> SimpleResolver {
            SimpleResolver {
                files: Files::new(),
                file_cache: HashMap::new(),
                term_cache: HashMap::new(),
            }
        }

        /// Add a mockup file to available imports.
        pub fn add_source(&mut self, name: String, source: String) {
            let id = self.files.add(name.clone(), source);
            self.file_cache.insert(name, id);
        }
    }

    impl ImportResolver for SimpleResolver {
        fn resolve(
            &mut self,
            path: &OsStr,
            _parent: Option<PathBuf>,
            pos: &Option<RawSpan>,
        ) -> Result<(ResolvedTerm, FileId), ImportError> {
            let file_id = self
                .file_cache
                .get(path.to_string_lossy().as_ref())
                .map(|id| id.clone())
                .ok_or(ImportError::IOError(
                    path.to_string_lossy().into_owned(),
                    String::from("Import not found by the mockup resolver."),
                    pos.clone(),
                ))?;

            if self.term_cache.contains_key(&file_id) {
                Ok((ResolvedTerm::FromCache(), file_id))
            } else {
                self.term_cache.insert(file_id, None);
                let buf = self.files.source(file_id);
                let term = parser::grammar::TermParser::new()
                    .parse(file_id, Lexer::new(&buf))
                    .map_err(|e| ParseError::from_lalrpop(e, file_id))
                    .map_err(|e| ImportError::ParseError(e, pos.clone()))?;
                Ok((
                    ResolvedTerm::FromFile {
                        term,
                        path: PathBuf::new(),
                    },
                    file_id,
                ))
            }
        }

        fn insert(&mut self, file_id: FileId, term: RichTerm) {
            self.term_cache.insert(file_id, Some(term));
        }

        fn get(&self, file_id: FileId) -> Option<RichTerm> {
            self.term_cache
                .get(&file_id)
                .map(|opt| opt.as_ref())
                .flatten()
                .cloned()
        }

        fn get_id(&self, path: &OsStr, _parent: Option<PathBuf>) -> Option<FileId> {
            self.file_cache
                .get(path.to_string_lossy().as_ref())
                .copied()
        }
    }
}
