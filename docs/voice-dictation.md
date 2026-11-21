# Voice dictation

Terminal and web capture use `Ctrl+Shift+S` (`Ctrl+S` on macOS); the web client also provides a microphone button and displays a live waveform. Stopping normally transcribes and inserts text at the current composer cursor, or at the end of an unfocused web draft. Pressing Enter in an active terminal recording, or Send in an active web recording, transcribes and submits immediately. Pressing `Esc` in either client, or using the web discard button, stops recording and deletes the captured audio without transcribing it.

With ChatGPT OAuth, dictation uses ChatGPT's private subscription transcription endpoint. This is not a public compatibility contract and may change without notice. API-key transcription uses the official provider's `/audio/transcriptions` endpoint. Microphone permission is required.
