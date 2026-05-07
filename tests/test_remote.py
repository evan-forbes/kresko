from __future__ import annotations

from kresko_py.remote import tmux_kill_command, tmux_start_command


def test_tmux_command_rendering_quotes_session_and_logs():
    command = tmux_start_command("txblast", "kresko txblast-local", "/root/kresko tx.log")

    assert "tmux new-session" in command
    assert "txblast" in command
    assert "'/root/kresko tx.log'" in command


def test_tmux_kill_command():
    assert tmux_kill_command("app") == "tmux kill-session -t app"
