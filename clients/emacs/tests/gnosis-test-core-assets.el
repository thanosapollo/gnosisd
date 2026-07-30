;;; gnosis-test-core-assets.el --- Managed asset tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'cl-lib)
(require 'gnosis-core-assets)

(ert-deftest gnosis-core-assets-rejects-unsafe-reference ()
  (dolist (reference '("https://example.org/file.png"
                       "/v1/assets/bad/file.png"
                       "assets/bad/file.png"
                       "assets/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/../bad"))
    (should-error (gnosis-core-assets-resolve reference)
                  :type 'gnosis-core-validation-error)))

(ert-deftest gnosis-core-assets-fetches-verifies-and-caches ()
  (let* ((bytes (string-to-unibyte "asset bytes"))
         (digest (secure-hash 'sha256 bytes))
         (directory (make-temp-file "gnosis-assets-" t))
         (gnosis-core-assets-cache-directory directory)
         (calls 0))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core--request)
                   (lambda (_method path &rest _)
                     (setq calls (1+ calls))
                     (should (equal
                              (format "/v1/assets/%s/image.png" digest) path))
                     bytes)))
          (let ((path (gnosis-core-assets-fetch digest "image.png")))
            (should (file-regular-p path))
            (should (= #o600 (file-modes path)))
            (should (equal digest (gnosis-core-assets--file-digest path)))
            (should (equal path (gnosis-core-assets-fetch digest "image.png")))
            (should (= calls 1))))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-rejects-digest-mismatch ()
  (let ((directory (make-temp-file "gnosis-assets-" t))
        (gnosis-core-assets-cache-directory nil))
    (setq gnosis-core-assets-cache-directory directory)
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core--request)
                   (lambda (&rest _) (string-to-unibyte "wrong"))))
          (should-error
           (gnosis-core-assets-fetch (make-string 64 ?a) "image.png")
           :type 'gnosis-core-protocol-error))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-replaces-invalid-cache-entry ()
  (let* ((bytes (string-to-unibyte "correct"))
         (digest (secure-hash 'sha256 bytes))
         (directory (make-temp-file "gnosis-assets-" t))
         (gnosis-core-assets-cache-directory directory)
         (path (gnosis-core-assets--path digest "asset.bin")))
    (unwind-protect
        (progn
          (make-directory (file-name-directory path) t)
          (write-region "wrong" nil path nil 'silent)
          (cl-letf (((symbol-function 'gnosis-core--request)
                     (lambda (&rest _) bytes)))
            (should (equal path (gnosis-core-assets-fetch digest "asset.bin")))
            (should (equal digest (gnosis-core-assets--file-digest path)))))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-resolves-server-logical-reference ()
  (let ((digest (make-string 64 ?a)))
    (cl-letf (((symbol-function 'gnosis-core-assets-fetch)
              (lambda (actual-digest name)
                 (should (equal digest actual-digest))
                 (should (equal "image%20name.png" name))
                 "/cache/image.png")))
      (should (equal "/cache/image.png"
                     (gnosis-core-assets-resolve
                      (format "assets/%s/image%%20name.png" digest)))))))

(ert-deftest gnosis-core-assets-encodes-literal-url-delimiters ()
  (let* ((bytes (string-to-unibyte "asset bytes"))
         (digest (secure-hash 'sha256 bytes))
         (directory (make-temp-file "gnosis-assets-" t))
         (gnosis-core-assets-cache-directory directory)
         request-path)
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core--request)
                   (lambda (_method path &rest _)
                     (setq request-path path)
                     bytes)))
          (gnosis-core-assets-resolve
           (format "assets/%s/image?#.png" digest))
          (should (equal
                   (format "/v1/assets/%s/image%%3F%%23.png" digest)
                   request-path)))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-rejects-symlinked-cache-directory ()
  (let* ((root (make-temp-file "gnosis-assets-root-" t))
         (outside (make-temp-file "gnosis-assets-outside-" t))
         (digest (make-string 64 ?a))
         (gnosis-core-assets-cache-directory root))
    (unwind-protect
        (progn
          (make-symbolic-link outside (expand-file-name digest root))
          (cl-letf (((symbol-function 'gnosis-core--request)
                     (lambda (&rest _) (ert-fail "unsafe cache fetched"))))
            (should-error (gnosis-core-assets-fetch digest "image.png")
                          :type 'gnosis-core-validation-error)))
      (delete-directory root t)
      (delete-directory outside t))))

(ert-deftest gnosis-core-assets-enforces-size-bounds ()
  (let* ((gnosis-core-assets--maximum-bytes 4)
         (directory (make-temp-file "gnosis-assets-" t))
         (gnosis-core-assets-cache-directory directory)
         (bytes (string-to-unibyte "four"))
         (digest (secure-hash 'sha256 bytes))
         (path (gnosis-core-assets--path digest "asset.bin")))
    (unwind-protect
        (progn
          (should (equal path (gnosis-core-assets--write path bytes digest)))
          (should-error
           (gnosis-core-assets--write path (string-to-unibyte "large") digest)
           :type 'gnosis-core-protocol-error))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-rejects-symlinked-cache-root-and-destination ()
  (let* ((parent (make-temp-file "gnosis-assets-parent-" t))
         (outside (make-temp-file "gnosis-assets-outside-" t))
         (root (expand-file-name "cache" parent))
         (digest (make-string 64 ?a))
         (gnosis-core-assets-cache-directory root))
    (unwind-protect
        (progn
          (make-symbolic-link outside root)
          (should-error (gnosis-core-assets-fetch digest "image.png")
                        :type 'gnosis-core-validation-error)
          (delete-file root)
          (make-directory (expand-file-name digest root) t)
          (make-symbolic-link
           (expand-file-name "outside.png" outside)
           (gnosis-core-assets--path digest "image.png"))
          (should-error (gnosis-core-assets-fetch digest "image.png")
                        :type 'gnosis-core-validation-error))
      (delete-directory parent t)
      (delete-directory outside t))))

(ert-deftest gnosis-core-assets-cleans-temporary-file-after-failure ()
  (let* ((directory (make-temp-file "gnosis-assets-" t))
         (gnosis-core-assets-cache-directory directory)
         (digest (make-string 64 ?a))
         (path (gnosis-core-assets--path digest "asset.bin")))
    (unwind-protect
        (progn
          (should-error
           (gnosis-core-assets--write
            path (string-to-unibyte "wrong") digest)
           :type 'gnosis-core-protocol-error)
          (should-not
           (directory-files (file-name-directory path) nil "\\`.asset-")))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-keeps-cache-path-confined ()
  (let* ((directory (make-temp-file "gnosis-assets-" t))
         (gnosis-core-assets-cache-directory directory)
         (digest (make-string 64 ?a)))
    (unwind-protect
        (dolist (name '("../escape" "/escape" "sub/escape" "." ".."))
          (should-error (gnosis-core-assets--path digest name)
                        :type 'gnosis-core-validation-error))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-rejects-directory-cache-destination ()
  (let* ((directory (make-temp-file "gnosis-assets-" t))
         (gnosis-core-assets-cache-directory directory)
         (digest (make-string 64 ?a))
         (path (gnosis-core-assets--path digest "asset.bin")))
    (unwind-protect
        (progn
          (make-directory path t)
          (should-error (gnosis-core-assets-fetch digest "asset.bin")
                        :type 'gnosis-core-validation-error))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-leaves-no-temporary-file-after-transport-failure ()
  (let* ((directory (make-temp-file "gnosis-assets-" t))
         (gnosis-core-assets-cache-directory directory)
         (digest (make-string 64 ?a))
         (digest-directory (expand-file-name digest directory)))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core--request)
                   (lambda (&rest _arguments)
                     (signal 'gnosis-core-timeout '("test timeout")))))
          (should-error (gnosis-core-assets-fetch digest "asset.bin")
                        :type 'gnosis-core-timeout)
          (should-not (and (file-directory-p digest-directory)
                           (directory-files digest-directory nil "\\`.asset-"))))
      (delete-directory directory t))))

(ert-deftest gnosis-core-assets-repairs-valid-cache-permissions ()
  (let* ((root (make-temp-file "gnosis-assets" t))
         (bytes "private")
         (digest (secure-hash 'sha256 bytes))
         (gnosis-core-assets-cache-directory root)
         (path (gnosis-core-assets--path digest "private.bin"))
         (directory (file-name-directory path)))
    (unwind-protect
        (progn
          (make-directory directory t)
          (let ((coding-system-for-write 'no-conversion))
            (write-region bytes nil path nil 'silent))
          (set-file-modes root #o755)
          (set-file-modes directory #o755)
          (set-file-modes path #o644)
          (cl-letf (((symbol-function 'gnosis-core--request)
                     (lambda (&rest _) (ert-fail "cache hit fetched network"))))
            (should (equal (gnosis-core-assets-fetch digest "private.bin") path)))
          (should (= (logand (file-modes root) #o777) #o700))
          (should (= (logand (file-modes directory) #o777) #o700))
          (should (= (logand (file-modes path) #o777) #o600)))
      (delete-directory root t))))

(provide 'gnosis-test-core-assets)
;;; gnosis-test-core-assets.el ends here
