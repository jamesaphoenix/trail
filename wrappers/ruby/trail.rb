# frozen_string_literal: true

# Thin Ruby wrapper around the `trail` coverage-scheduler CLI.
#
# Shells out to the `trail` binary and parses its JSON; the CLI is the source of
# truth. The binary is located via the TRAIL_BIN environment variable, else
# `trail` on PATH.
#
#   require_relative "trail"
#   c = Trail::Client.new("/repo")
#   c.init
#   while (folder = c.claim("refine", agent: "a1"))
#     # ... investigate folder["path"] ...
#     c.done("refine", folder["path"], agent: "a1", found: 3)
#   end
#   p c.status("refine")

require "json"
require "open3"

module Trail
  EXIT_OK = 0
  EXIT_ERROR = 1
  EXIT_SWEEP_COMPLETE = 3
  EXIT_NONE_AVAILABLE = 4

  class Error < StandardError; end

  def self.bin
    ENV["TRAIL_BIN"] || "trail"
  end

  # A coverage-scheduler handle bound to a project root.
  class Client
    def initialize(root = nil, poll: 2.0)
      @root = root
      @poll = poll
    end

    def init
      run("init").last
    end

    # Claim the next folder. Returns the folder hash, or nil when the sweep is
    # complete. Blocks and retries while folders are only leased elsewhere.
    def claim(task, agent: nil)
      args = ["next", "--task", task]
      args += ["--agent", agent] if agent
      loop do
        code, data = run(*args)
        return data if code == EXIT_OK
        return nil if code == EXIT_SWEEP_COMPLETE

        sleep(@poll) if code == EXIT_NONE_AVAILABLE
      end
    end

    def done(task, path, agent: nil, found: nil, clean: false)
      complete("done", task, path, agent: agent, found: found, clean: clean)
    end

    def skip(task, path, agent: nil, reason: nil, found: nil, clean: false)
      complete("skip", task, path, agent: agent, reason: reason, found: found, clean: clean)
    end

    def status(task)
      run("status", "--task", task).last
    end

    private

    def outcome_args(found, clean)
      return ["--clean"] if clean
      return ["--found", found.to_s] unless found.nil?

      []
    end

    def complete(verb, task, path, agent:, found:, clean:, reason: nil)
      args = [verb, "--task", task, "--path", path]
      args += ["--agent", agent] if agent
      args += ["--reason", reason] if reason
      args += outcome_args(found, clean)
      run(*args).last
    end

    # Returns [exit_code, parsed_json].
    def run(*args)
      cmd = [Trail.bin]
      cmd += ["--root", @root] if @root
      cmd += args
      stdout, stderr, status = Open3.capture3(*cmd)
      code = status.exitstatus
      out = stdout.strip
      data = out.empty? ? {} : JSON.parse(out.lines.last)
      if code == EXIT_ERROR
        msg = (data.is_a?(Hash) && data["error"]) || stderr.strip
        raise Error, (msg.empty? ? "trail error" : msg)
      end
      [code, data]
    end
  end
end
