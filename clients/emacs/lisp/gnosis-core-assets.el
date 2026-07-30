;;; gnosis-core-assets.el --- Managed Gnosis asset cache  -*- lexical-binding: t; -*-

;; Copyright (C) 2026  Free Software Foundation, Inc.
;; Author: Thanos Apollo <public@thanosapollo.org>
;; Keywords: extensions

;; This file is free software; you can redistribute it and/or modify it
;; under the terms of the GNU General Public License as published by the
;; Free Software Foundation, either version 3 of the License, or (at your
;; option) any later version.

;;; Commentary:

;; Fetch authenticated managed assets from gnosisd, verify their SHA-256
;; digest, and cache them privately for presentation.

;;; Code:

(require 'gnosis-core)
(require 'subr-x)
(require 'url-util)

(defgroup gnosis-core-assets nil
  "Managed Gnosis presentation assets."
  :group 'gnosis-core
  :prefix "gnosis-core-assets-")

(defcustom gnosis-core-assets-cache-directory
  (locate-user-emacs-file "gnosis/assets/")
  "Private directory for digest-verified presentation assets."
  :type 'directory
  :group 'gnosis-core-assets)

(defconst gnosis-core-assets--maximum-bytes (* 32 1024 1024)
  "Maximum accepted managed asset size.")

(defun gnosis-core-assets--digest-p (digest)
  "Return non-nil when DIGEST is a lowercase SHA-256 hex string."
  (and (stringp digest)
       (string-match-p "\\`[0-9a-f]\\{64\\}\\'" digest)))

(defun gnosis-core-assets--name-p (name)
  "Return non-nil when NAME is a safe single path segment."
  (and (stringp name) (not (string-empty-p name))
       (equal name (file-name-nondirectory name))
       (not (member name '("." "..")))
       (not (string-match-p "[\0/\\]" name))))

(defun gnosis-core-assets--validate (digest name)
  "Signal when managed asset DIGEST or NAME is invalid."
  (unless (and (gnosis-core-assets--digest-p digest)
               (gnosis-core-assets--name-p name))
    (signal 'gnosis-core-validation-error '("Invalid managed asset reference"))))

(defun gnosis-core-assets--path (digest name)
  "Return private cache path for DIGEST and NAME."
  (gnosis-core-assets--validate digest name)
  (expand-file-name name (expand-file-name digest gnosis-core-assets-cache-directory)))

(defun gnosis-core-assets--file-digest (path)
  "Return SHA-256 digest for bytes in PATH."
  (with-temp-buffer
    (set-buffer-multibyte nil)
    (insert-file-contents-literally path)
    (secure-hash 'sha256 (current-buffer))))

(defun gnosis-core-assets--valid-file-p (path digest)
  "Return non-nil when PATH is a private regular file matching DIGEST."
  (and (file-regular-p path)
       (not (file-symlink-p path))
       (<= (file-attribute-size (file-attributes path))
           gnosis-core-assets--maximum-bytes)
       (equal digest (gnosis-core-assets--file-digest path))))

(defun gnosis-core-assets--enforce-private-mode (path mode)
  "Require PATH ownership and restore its permissions to MODE."
  (let ((attributes (file-attributes path 'integer)))
    (unless (and attributes
                 (equal (file-attribute-user-id attributes) (user-uid)))
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache ownership")))
    (condition-case nil
        (set-file-modes path mode)
      (file-error
       (signal 'gnosis-core-validation-error
               '("Cannot secure managed asset cache permissions"))))
    (unless (= (logand (file-modes path) #o777) mode)
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache permissions")))))

(defun gnosis-core-assets--write (path bytes digest)
  "Write BYTES atomically to PATH and verify DIGEST."
  (when (> (string-bytes bytes) gnosis-core-assets--maximum-bytes)
    (signal 'gnosis-core-protocol-error '("Managed asset exceeds size limit")))
  (let* ((root (directory-file-name
                (expand-file-name gnosis-core-assets-cache-directory)))
         (directory (file-name-directory path))
         (temporary nil))
    (when (file-symlink-p root)
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache root")))
    (make-directory root t)
    (when (file-symlink-p root)
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache root")))
    (gnosis-core-assets--enforce-private-mode root #o700)
    (when (file-symlink-p (directory-file-name directory))
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache directory")))
    (make-directory directory t)
    (when (file-symlink-p (directory-file-name directory))
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache directory")))
    (gnosis-core-assets--enforce-private-mode directory #o700)
    (unwind-protect
        (progn
          (setq temporary (make-temp-file (expand-file-name ".asset-" directory)))
          (let ((coding-system-for-write 'no-conversion)
                (write-region-inhibit-fsync nil))
            (write-region bytes nil temporary nil 'silent))
          (set-file-modes temporary #o600)
          (unless (equal digest (gnosis-core-assets--file-digest temporary))
            (signal 'gnosis-core-protocol-error '("Managed asset digest mismatch")))
          (rename-file temporary path t)
          (gnosis-core-assets--enforce-private-mode path #o600)
          (setq temporary nil)
          path)
      (when (and temporary (file-exists-p temporary))
        (delete-file temporary)))))

(defun gnosis-core-assets-fetch (digest name)
  "Fetch DIGEST and NAME through authenticated HTTP, returning cache path."
  (let ((path (gnosis-core-assets--path digest name)))
    (when (file-symlink-p
           (directory-file-name
            (expand-file-name gnosis-core-assets-cache-directory)))
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache root")))
    (when (file-symlink-p
           (directory-file-name (file-name-directory path)))
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache directory")))
    (when (file-symlink-p path)
      (signal 'gnosis-core-validation-error
              '("Unsafe managed asset cache destination")))
    (if (gnosis-core-assets--valid-file-p path digest)
        (progn
          (gnosis-core-assets--enforce-private-mode
           (directory-file-name
            (expand-file-name gnosis-core-assets-cache-directory))
           #o700)
          (gnosis-core-assets--enforce-private-mode
           (directory-file-name (file-name-directory path)) #o700)
          (gnosis-core-assets--enforce-private-mode path #o600)
          path)
      (when (file-directory-p path)
        (signal 'gnosis-core-validation-error
                '("Unsafe managed asset cache destination")))
      (when (file-exists-p path) (delete-file path))
      (gnosis-core-assets--write
       path
       (gnosis-core--request
        "GET" (format "/v1/assets/%s/%s" digest (url-hexify-string name))
        nil 200 t t t)
       digest))))

(defun gnosis-core-assets-resolve (reference)
  "Resolve managed asset REFERENCE to a verified private cache path.
REFERENCE must be a canonical `assets/DIGEST/NAME' logical path."
  (unless (and (stringp reference)
               (string-match
                "\\`assets/\\([0-9a-f]\\{64\\}\\)/\\([^/]+\\)\\'"
                reference))
    (signal 'gnosis-core-validation-error '("Invalid managed asset URL")))
  (gnosis-core-assets-fetch
   (match-string 1 reference)
   (match-string 2 reference)))

(provide 'gnosis-core-assets)
;;; gnosis-core-assets.el ends here
