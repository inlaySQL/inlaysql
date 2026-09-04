# frozen_string_literal: true

# InlaySQL — the Ruby wrapper over the C ABI.
#
# One file, one gem (`ffi`): copy it into your project (or require it from
# the release archive) and open a database like SQLite — no server, the file
# is yours.
#
#   require 'inlaysql'
#
#   InlaySQL.connect('app.inlay') do |db|          # creates if absent
#     db.run 'CREATE TABLE IF NOT EXISTS users (
#         id INTEGER PRIMARY KEY, name TEXT)'
#     db.run 'INSERT INTO users (name) VALUES (?)', ['Ada']
#
#     db.run('SELECT * FROM users')                # {"columns"=>…, "rows"=>…}
#     db.query('SELECT id, name FROM users').each do |row|
#       puts row['name']                           # rows as hashes
#     end
#   end                                            # handle closed here
#
# Read-only: InlaySQL.connect('app.inlay', readonly: true) — the file must
# already exist and every write is refused. Without a block, call #close
# yourself.
#
# One handle is one thread at a time (open one per thread), the same rule
# SQLite's default connection has. Vector parameters bind as a plain Ruby
# array of numbers; vector cells come back as the placeholder "<vector(n)>" —
# the raw floats do not cross the boundary in JSON.
#
# Gem requirement: gem install ffi. Tested against libinlaysql_ffi from
# inlaySQL/inlaysql v0.0.1; the C surface it wraps is documented in
# include/inlaysql.h beside this file.

require 'ffi'
require 'json'

class InlaySQL
  class Error < StandardError; end

  INLAYSQL_OK = 0
  INLAYSQL_ERR_BAD_HANDLE = 2

  LIBRARY_NAMES = {
    darwin: 'libinlaysql_ffi.dylib',
    windows: 'inlaysql_ffi.dll',
    linux: 'libinlaysql_ffi.so',
  }.freeze

  def self.linux?
    RbConfig::CONFIG['host_os'].include?('linux')
  end

  # Find the library beside this file, then in the working directory.
  def self.library_path_impl
    name = LIBRARY_NAMES.values.find do |candidate|
      File.file?(File.join(__dir__, candidate)) || File.file?(File.join(Dir.pwd, candidate))
    end || LIBRARY_NAMES[linux? ? :linux : :darwin]
    candidates = [__dir__, Dir.pwd].map { |dir| File.join(dir, name) }
    found = candidates.find { |path| File.file?(path) }
    raise Error, <<~MSG if found.nil?
      could not find #{name} beside #{__dir__} or the working directory —
      pass lib:, or download it from
      https://github.com/inlaySQL/inlaysql/releases
    MSG
    found
  end

  # The C surface, bound lazily. `attach_function` needs the library loaded
  # first, and where the library lives is the *caller's* decision — so the
  # function objects are built in `attach_library`, called by `connect`,
  # not at require time.
  module Native
    extend FFI::Library

    class << self
      def attach(path)
        ffi_lib path
        attach_function :inlaysql_open, [:string], :pointer
        attach_function :inlaysql_open_read_only, [:string], :pointer
        attach_function :inlaysql_close, [:pointer], :void
        attach_function :inlaysql_exec, [:pointer, :string, :string, :pointer], :int
        attach_function :inlaysql_last_error, [], :string
        attach_function :inlaysql_free_string, [:pointer], :void
        attach_function :inlaysql_version, [], :string
      end
    end
  end

  # Bind Native's functions to the library at `path` (default: the search
  # that library_path_impl performs). Called by connect() before any call;
  # safe to call again.
  def self.attach_library(path = nil)
    Native.attach(path || library_path_impl)
  end

  def self.version
    Native.inlaysql_version
  end

  # Open the database file at `path`, creating it if it does not exist.
  # With a block, yields the handle and closes it; without, returns it
  # (call #close yourself).
  def self.connect(path, lib: nil, readonly: false, &block)
    attach_library(lib)
    db = allocate
    db.send(:initialize!, path, readonly)
    return db unless block

    begin
      yield db
    ensure
      db.close
    end
  end

  def initialize!(path, readonly)
    open_fn = readonly ? :inlaysql_open_read_only : :inlaysql_open
    @handle = Native.send(open_fn, path)
    raise Error, "open failed: #{Native.inlaysql_last_error}" if @handle.null?
  end

  def version
    Native.inlaysql_version
  end

  # Run one statement. Returns {"kind"=>"ddl"},
  # {"kind"=>"written","rows"=>n}, or {"columns"=>[…],"rows"=>[[…],…]} for a
  # SELECT. Params bind to `?` in order; an array of numbers is a vector.
  def run(sql, params = nil)
    out = FFI::MemoryPointer.new(:pointer)
    code = Native.inlaysql_exec(@handle, sql, params && JSON.generate(params), out)
    case code
    when INLAYSQL_ERR_BAD_HANDLE then raise Error, 'bad handle'
    when INLAYSQL_OK
      result = JSON.parse(out.read_pointer.read_string)
      Native.inlaysql_free_string(out.read_pointer)
      result
    else
      raise Error, "#{Native.inlaysql_last_error} — while running: #{sql}"
    end
  end

  # Run a SELECT; rows as hashes keyed by column name.
  def query(sql, params = nil)
    result = run(sql, params)
    raise Error, "not a query: #{sql}" unless result.key?('columns')

    result['rows'].map { |row| result['columns'].zip(row).to_h }
  end

  # First row as a hash, or nil.
  def first(sql, params = nil)
    query(sql, params).first
  end

  def close
    return if @handle.nil? || @handle.null?

    Native.inlaysql_close(@handle)
    @handle = nil
  end
end
