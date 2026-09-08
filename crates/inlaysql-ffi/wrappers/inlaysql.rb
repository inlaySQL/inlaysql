# frozen_string_literal: true

# InlaySQL — the Ruby client over the C ABI.
#
# One file, one gem (`ffi`): copy it into your project (or require it from
# the release archive) and open a database like SQLite — no server, the file
# is yours.
#
#   require 'inlaysql'
#
#   InlaySQL.connect('app.inlay') do |db|                      # creates if absent
#     db.execute 'CREATE TABLE IF NOT EXISTS users (
#         id INTEGER PRIMARY KEY, name TEXT, email TEXT)'
#
#     id = db.insert('users', name: 'Ada', email: 'ada@example.org')
#
#     db.query('SELECT id, name FROM users WHERE id > :after', after: 0).each do |user|
#       puts user['name']                                        # rows as hashes
#     end
#     ada   = db.first('SELECT * FROM users WHERE id = ?', [id])   # one row, or nil
#     count = db.value('SELECT COUNT(*) FROM users')               # one cell
#     names = db.column('SELECT name FROM users ORDER BY name')   # one column
#
#     result = db.execute('UPDATE users SET name = ? WHERE id = ?', ['Ada L.', id])
#     result.rows_affected                                       # 1
#
#     db.transaction do |db|                                     # BEGIN … COMMIT; ROLLBACK
#       db.insert('users', name: 'Grace')                        # on raise; rerun on a
#       db.insert('users', name: 'Linus')                        # write conflict
#     end
#   end                                                          # handle closed here
#
# Parameters: positional `?` with an array, or named `:name` with a hash (or
# keyword arguments). An array of numbers binds as a vector, so a retrieval
# call is
#
#   db.query('SELECT id, vector_score(embedding, ?) AS s FROM docs ORDER BY s LIMIT 10',
#            [embedding])
#
# Errors: InlaySQL::ConstraintError (unique/not-null…), InlaySQL::ConflictError
# (another writer committed first — retry), InlaySQL::UnsupportedError (a
# clause the engine refuses rather than ignores), InlaySQL::Error for the
# rest; the engine's own message is the exception message.
#
# Read-only: InlaySQL.connect('app.inlay', readonly: true) — the file must
# already exist and every write is refused. Without a block, call #close
# yourself.
#
# One handle is one thread at a time (open one per thread or process), and
# every one of them may write — concurrent commits to one file are what this
# engine does that SQLite does not. The library is attached once per
# process. Vector cells come back as the placeholder "<vector(n)>" — the raw
# floats do not cross the boundary in JSON.
#
# Run this file itself for its self-test: ruby inlaysql.rb [path/to/lib].
#
# Gem requirement: gem install ffi. Tested against libinlaysql_ffi from
# inlaySQL/inlaysql v0.0.6; the C surface it wraps is documented in
# include/inlaysql.h beside this file.

require 'ffi'
require 'json'

class InlaySQL
  # The engine's own message, verbatim — there are no numeric codes.
  class Error < StandardError; end
  # UNIQUE, NOT NULL, CHECK, FOREIGN KEY — the row was refused.
  class ConstraintError < Error; end
  # Another handle committed first; nothing was written. Safe to retry.
  class ConflictError < Error; end
  # A clause this engine refuses rather than silently ignores.
  class UnsupportedError < Error; end

  INLAYSQL_OK = 0
  INLAYSQL_ERR_BAD_HANDLE = 2

  LIBRARY_NAMES = {
    darwin: 'libinlaysql_ffi.dylib',
    windows: 'inlaysql_ffi.dll',
    linux: 'libinlaysql_ffi.so',
  }.freeze

  # What a statement did. `rows` is set only when the statement was a query.
  # `last_insert_id` is the most recent INSERT's row id on this handle
  # (SQLite's `last_insert_rowid()` contract), or nil before any INSERT.
  Result = Struct.new(:rows_affected, :last_insert_id, :ddl, :rows, keyword_init: true) do
    def ddl? = ddl
  end

  # A query's answer. Enumerable over hashes keyed by column name; `size` is
  # the row count; the accessors answer the common shapes without a loop.
  class Rows
    include Enumerable

    attr_reader :columns

    def initialize(columns, rows)
      @columns = columns
      @rows = rows
    end

    def each
      return enum_for(:each) unless block_given?

      @rows.each { |row| yield @columns.zip(row).to_h }
    end

    def size = @rows.size
    alias length size
    alias count size

    def empty? = @rows.empty?

    # Every row as a hash.
    def all = to_a

    # The rows exactly as the ABI returned them, positional.
    def raw = @rows

    # First row as a hash, or nil.
    def first = @rows.empty? ? nil : @columns.zip(@rows[0]).to_h

    # The first cell of the first row, or nil when there is no row.
    def value = @rows.dig(0, 0)

    # One column, by index or name, across every row.
    def column(column = 0)
      index = column.is_a?(Integer) ? column : @columns.index(column.to_s)
      raise Error, "no such column: #{column}" if index.nil? || index >= @columns.size

      @rows.map { |row| row[index] }
    end
  end

  # The C surface, bound lazily: where the library lives is the caller's
  # decision, so the functions are attached in `attach_library`, called by
  # `connect`, not at require time.
  module Native
    extend FFI::Library

    class << self
      attr_reader :attached_path

      def attach(path)
        return if @attached_path == path

        ffi_lib path
        attach_function :inlaysql_open, [:string], :pointer
        attach_function :inlaysql_open_read_only, [:string], :pointer
        attach_function :inlaysql_close, [:pointer], :void
        attach_function :inlaysql_exec, %i[pointer string string pointer], :int
        attach_function :inlaysql_last_error, [], :string
        attach_function :inlaysql_free_string, [:pointer], :void
        attach_function :inlaysql_version, [], :string
        @attached_path = path
      end
    end
  end

  class << self
    # Open the database file at `path`, creating it if it does not exist.
    # With a block, yields the handle and closes it; without, returns it
    # (call #close yourself). `readonly: true` refuses every write and needs
    # the file to exist. `lib:` names the library; by default it is looked
    # up beside this file, its parent, then the working directory.
    def connect(path, lib: nil, readonly: false)
      attach_library(lib)
      db = new(path, readonly)
      return db unless block_given?

      begin
        yield db
      ensure
        db.close
      end
    end

    # Bind Native's functions to the library at `path`. Called by connect().
    def attach_library(path = nil)
      Native.attach(path || locate_library)
    end

    def version
      Native.inlaysql_version
    end

    private

    def locate_library
      host = RbConfig::CONFIG['host_os']
      name = if host.include?('linux') then LIBRARY_NAMES[:linux]
             elsif host =~ /mswin|mingw/ then LIBRARY_NAMES[:windows]
             else LIBRARY_NAMES[:darwin]
             end
      candidates = [__dir__, File.join(__dir__, '..'), Dir.pwd].map { |dir| File.join(dir, name) }
      found = candidates.find { |candidate| File.file?(candidate) }
      raise Error, <<~MSG if found.nil?
        could not find #{name} beside #{__dir__} or the working directory —
        pass lib:, or download it from
        https://github.com/inlaySQL/inlaysql/releases
      MSG

      found
    end
  end

  def initialize(path, readonly = false)
    open_fn = readonly ? :inlaysql_open_read_only : :inlaysql_open
    @handle = Native.send(open_fn, path.to_s)
    raise Error, "open failed: #{Native.inlaysql_last_error}" if @handle.null?

    @depth = 0
  end

  def version
    Native.inlaysql_version
  end

  # ---- statements ---------------------------------------------------------

  # Run any statement. For a write, the result says how many rows and which
  # id; for a query, prefer #query.
  def execute(sql, params = nil, **named)
    raw = run(sql, params, **named)
    if raw.key?('columns')
      Result.new(rows_affected: 0, last_insert_id: nil, ddl: false,
                 rows: Rows.new(raw['columns'], raw['rows']))
    else
      Result.new(rows_affected: raw.fetch('rows', 0), last_insert_id: raw['last_insert_id'],
                 ddl: raw['kind'] == 'ddl', rows: nil)
    end
  end

  # Run a query. Enumerate the result, or ask it for #all, #first, #value,
  # #column.
  def query(sql, params = nil, **named)
    raw = run(sql, params, **named)
    raise Error, "not a query: #{sql}" unless raw.key?('columns')

    Rows.new(raw['columns'], raw['rows'])
  end

  # First row as a hash, or nil.
  def first(sql, params = nil, **named) = query(sql, params, **named).first

  # The first cell of the first row, or nil when there is no row.
  def value(sql, params = nil, **named) = query(sql, params, **named).value

  # One column across every row.
  def column(sql, params = nil, column: 0, **named) = query(sql, params, **named).column(column)

  # Insert one row given as a hash or keyword arguments; return its row id.
  def insert(table, row = nil, **values)
    merged = (row || {}).merge(values)
    raise Error, "insert into #{table}: no columns given" if merged.empty?

    columns = merged.keys.map { |name| quote_identifier(name) }.join(', ')
    marks = (['?'] * merged.size).join(', ')
    result = execute("INSERT INTO #{quote_identifier(table)} (#{columns}) VALUES (#{marks})",
                     merged.values)
    raise Error, "insert into #{table} reported no row id" if result.last_insert_id.nil?

    result.last_insert_id
  end

  # ---- transactions -------------------------------------------------------

  # Yield inside BEGIN … COMMIT. A raise rolls back and re-raises. A write
  # conflict (another handle committed first) rolls back and reruns the
  # block, up to `retries` times, so the block must be safe to repeat.
  # Nested calls join the outer transaction.
  def transaction(retries: 3)
    if @depth.positive?
      @depth += 1
      begin
        return yield self
      ensure
        @depth -= 1
      end
    end

    attempt = 0
    loop do
      run('BEGIN')
      @depth = 1
      begin
        value = yield self
        run('COMMIT')
        @depth = 0
        return value
      rescue Exception => e # rubocop:disable Lint/RescueException — anything must roll back
        @depth = 0
        rollback_quietly
        raise unless e.is_a?(ConflictError) && attempt < retries

        attempt += 1
      end
    end
  end

  def in_transaction? = @depth.positive?

  # ---- the raw call -------------------------------------------------------

  # Run one statement and return the ABI's JSON, decoded:
  # {"kind"=>"ddl"}, {"kind"=>"written","rows"=>n,"last_insert_id"=>k}, or
  # {"columns"=>[…],"rows"=>[[…],…]}. The typed methods are built on this;
  # it stays public for callers that want the raw shape.
  def run(sql, params = nil, **named)
    raise Error, 'the database is closed' if @handle.nil?

    params = named if params.nil? && !named.empty?
    sql, params = self.class.bind_named(sql, params) if params.is_a?(Hash)
    encoded = params.nil? || params.empty? ? nil : JSON.generate(params)

    out = FFI::MemoryPointer.new(:pointer)
    code = Native.inlaysql_exec(@handle, sql, encoded, out)
    case code
    when INLAYSQL_OK
      pointer = out.read_pointer
      begin
        JSON.parse(pointer.read_string)
      ensure
        Native.inlaysql_free_string(pointer)
      end
    when INLAYSQL_ERR_BAD_HANDLE then raise Error, 'bad handle'
    else raise self.class.error(Native.inlaysql_last_error, sql)
    end
  end

  def close
    return if @handle.nil?

    Native.inlaysql_close(@handle)
    @handle = nil
  end

  # ---- helpers ------------------------------------------------------------

  class << self
    # Rewrite `:name` placeholders to `?` in the order they appear, skipping
    # string literals and quoted identifiers; return the values to match.
    def bind_named(sql, params)
      params = params.transform_keys(&:to_s)
      out = +''
      values = []
      i = 0
      n = sql.length
      while i < n
        c = sql[i]
        if ["'", '"', '`'].include?(c)
          j = i + 1
          while j < n
            if sql[j] == c
              if sql[j + 1] == c
                j += 2
                next
              end
              break
            end
            j += 1
          end
          out << sql[i..j]
          i = j + 1
        elsif c == ':' && sql[i + 1] =~ /[A-Za-z_]/
          j = i + 1
          j += 1 while j < n && sql[j] =~ /[A-Za-z0-9_]/
          name = sql[(i + 1)...j]
          raise error("no value bound for :#{name}", sql) unless params.key?(name)

          values << params[name]
          out << '?'
          i = j
        else
          out << c
          i += 1
        end
      end
      [out, values]
    end

    # The engine's message, as the error class its prefix names.
    def error(message, sql = nil)
      text = sql.nil? ? message : "#{message} — while running: #{sql}"
      klass = if message.start_with?('constraint failed') then ConstraintError
              elsif message.start_with?('write conflict') then ConflictError
              elsif message.start_with?('unsupported') then UnsupportedError
              else Error
              end
      klass.new(text)
    end
  end

  private

  def quote_identifier(name) = %("#{name.to_s.gsub('"', '""')}")

  def rollback_quietly
    run('ROLLBACK')
  rescue Error
    # A conflict at COMMIT already ended the transaction; nothing to undo.
  end
end

if $PROGRAM_NAME == __FILE__ # a self-test; pass the library path or keep it beside this file
  require 'tmpdir'

  Dir.mktmpdir do |dir|
    InlaySQL.connect(File.join(dir, 'selftest.inlay'), lib: ARGV[0]) do |db|
      raise 'version' if db.version.empty?

      ddl = db.execute('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT UNIQUE)')
      raise 'ddl' unless ddl.ddl? && ddl.rows.nil?
      raise 'insert 1' unless db.insert('t', name: 'Ada') == 1
      raise 'insert 2' unless db.insert('t', { 'name' => 'Grace' }) == 2

      written = db.execute('UPDATE t SET name = :n WHERE id = :id', id: 2, n: 'Grace H.')
      raise 'affected' unless written.rows_affected == 1 && written.last_insert_id == 2
      raise 'all' unless db.query('SELECT * FROM t ORDER BY id').all == [
        { 'id' => 1, 'name' => 'Ada' }, { 'id' => 2, 'name' => 'Grace H.' },
      ]
      raise 'count' unless db.query('SELECT id FROM t').size == 2
      raise 'first' unless db.first('SELECT name FROM t WHERE id = ?', [1]) == { 'name' => 'Ada' }
      raise 'first none' unless db.first('SELECT name FROM t WHERE id = ?', [99]).nil?
      raise 'value' unless db.value('SELECT COUNT(*) FROM t') == 2
      raise 'column' unless db.column('SELECT name FROM t ORDER BY name') == ['Ada', 'Grace H.']
      raise 'column by name' unless db.column('SELECT id, name FROM t ORDER BY id', column: 'name') == ['Ada', 'Grace H.']
      raise 'literal' unless db.value("SELECT name FROM t WHERE name = ':not_a_param'").nil?
      raise 'named' unless db.query('SELECT id FROM t WHERE id > :after', after: 1).size == 1

      begin
        db.insert('t', name: 'Ada')
        raise 'no constraint'
      rescue InlaySQL::ConstraintError
        nil
      end
      begin
        db.transaction do |tx|
          tx.insert('t', name: 'Linus')
          raise ArgumentError, 'abort'
        end
      rescue ArgumentError
        nil
      end
      raise 'rolled back' unless db.value('SELECT COUNT(*) FROM t') == 2
      raise 'txn value' unless db.transaction { |tx| tx.insert('t', name: 'Linus') } == 3
      raise 'nested' unless db.transaction { |tx| tx.transaction { |x| x.value('SELECT COUNT(*) FROM t') } } == 3

      begin
        db.execute('SELECT * FROM t WHERE name = :missing', other: 1)
        raise 'missing param'
      rescue InlaySQL::Error
        nil
      end
    end
  end
  puts 'inlaysql.rb self-test passed'
end
