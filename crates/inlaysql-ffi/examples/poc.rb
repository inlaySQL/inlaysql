#!/usr/bin/env ruby
# frozen_string_literal: true

# InlaySQL from Ruby, through the C ABI — a proof of concept.
#
# This is the "SQLite-like adapter" direction: no server, the file opened
# in-process through Ruby's `ffi` gem. It is the whole binding — nothing
# else is needed, which is the point.
#
# Build the library first (repo root):
#   cargo build -p inlaysql-ffi --release
#
# Run this:
#   gem install ffi          # the only gem; stdlib otherwise
#   ruby examples/poc.rb [path/to/libinlaysql.dylib|.so] [db path]

require "ffi"
require "json"
require "tmpdir"

# The binding. Fifteen lines — this is the whole thing, and the reason a C
# ABI is worth maintaining: every FFI language writes one of these and is
# done.
module InlaySQL
  LIB = ARGV[0] || File.expand_path(
    "../../../target/release/libinlaysql_ffi.#{RbConfig::CONFIG['SOEXT']}",
    __dir__
  )
  INLAYSQL_OK = 0
  INLAYSQL_ERR_BAD_HANDLE = 2

  module Native
    extend FFI::Library
    ffi_lib LIB

    attach_function :inlaysql_open, [:string], :pointer
    attach_function :inlaysql_open_read_only, [:string], :pointer
    attach_function :inlaysql_close, [:pointer], :void
    attach_function :inlaysql_exec, [:pointer, :string, :string, :pointer], :int
    attach_function :inlaysql_last_error, [], :string
    attach_function :inlaysql_free_string, [:pointer], :void
    attach_function :inlaysql_version, [], :string
  end

  def self.version
    Native.inlaysql_version
  end

  # A handle with a block form and a finalizer, so close is not a memory
  # leak waiting to happen. `exec` takes plain Ruby values: strings, ints,
  # floats, nil, arrays of numbers (vectors).
  class Database
    def initialize(path)
      @handle = Native.inlaysql_open(path)
      raise "open failed: #{Native.inlaysql_last_error}" if @handle.null?
    end

    def exec(sql, params = nil)
      out = FFI::MemoryPointer.new(:pointer)
      code = Native.inlaysql_exec(
        @handle, sql,
        params ? JSON.generate(params) : nil,
        out
      )
      case code
      when INLAYSQL_ERR_BAD_HANDLE then raise "bad handle"
      when INLAYSQL_OK
        result = JSON.parse(out.read_pointer.read_string)
        Native.inlaysql_free_string(out.read_pointer)
        result
      else
        raise "#{Native.inlaysql_last_error} — while running: #{sql}"
      end
    end

    def close
      Native.inlaysql_close(@handle)
      @handle = nil
    end
  end
end

lib_desc = InlaySQL::LIB
abort("library not found: #{lib_desc}\nbuild it: cargo build -p inlaysql-ffi --release") unless File.exist?(lib_desc)

db_path = ARGV[1] || File.join(Dir.tmpdir, "inlaysql-ruby-poc.inlay")

puts "InlaySQL engine version #{InlaySQL.version}"
puts "database: #{db_path}"
puts

db = InlaySQL::Database.new(db_path)
begin
  r = db.exec(<<~SQL)
    CREATE TABLE IF NOT EXISTS docs (
      id INTEGER PRIMARY KEY,
      title TEXT,
      body TEXT
    )
  SQL
  puts "create:      #{r['kind']}"

  r = db.exec("INSERT INTO docs (title, body) VALUES (?, ?)",
              ["Hello", "from Ruby over the C ABI — no server, the file is ours."])
  puts "insert:      #{r['kind']}, #{r['rows']} row"

  r = db.exec("SELECT id, title, body FROM docs ORDER BY id")
  puts "select:      #{r['rows'][0][0]} #{r['rows'][0][1]}"
  puts "             #{r['rows'][0][2]}"

  r = db.exec("SELECT COUNT(*) FROM docs")
  puts "count:       #{r['rows'][0][0]}"

  begin
    db.exec("SELECT * FROM no_such_table")
  rescue StandardError => e
    puts "error path:  InlaySQL error: #{e.message}"
  end
ensure
  db.close
end

puts
puts "OK — Ruby drove the engine in-process through the C ABI."
